//! Request-scoped Pascal source resolution.
//!
//! The resolver owns lookup policy and bounded observations, while callers own
//! project discovery state and provide a [`SourceStore`] for disk or overlay
//! bytes.  No result is considered precise unless the candidate was loaded and
//! its declaration matched the requested name.

use crate::compile_time::{AdmittedOwned, AdmittedShared, CompileTimeBudget};
use pascal_project::{
    MetadataObservation, ProjectContext, ProjectPathEntry, ProjectPathProvenance,
    ProjectReadObservation, ProjectReadStamp, ReadPolicy, content_hash_bytes, path_identity,
    path_stamp_result, read_package_metadata_with_observations,
};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::fs;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// A caller-owned cancellation source.
pub trait CancellationToken {
    fn is_cancelled(&self) -> bool;
}

impl CancellationToken for AtomicBool {
    fn is_cancelled(&self) -> bool {
        self.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NoCancellation;

impl CancellationToken for NoCancellation {
    fn is_cancelled(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolverLimits {
    pub max_dependency_units: usize,
    pub max_directory_entries: usize,
    pub max_scanned_bytes: usize,
    pub max_source_bytes: usize,
    pub max_include_files: usize,
    pub max_include_bytes: usize,
    pub max_include_directives: usize,
    pub max_include_depth: usize,
    pub max_package_lookups: usize,
    pub max_package_unit_candidates: usize,
    pub max_package_catalogue_entries: usize,
    pub max_package_catalogues: usize,
    pub max_resolution_warnings: usize,
    /// Aggregate compile-time scratch plus published outputs over this resolver.
    /// Published outputs remain reserved until the request-local resolver drops.
    pub max_compile_time_retained_bytes: usize,
    pub max_compile_time_work: usize,
    pub max_compile_time_byte_work: usize,
}

impl Default for ResolverLimits {
    fn default() -> Self {
        Self {
            max_dependency_units: 256,
            max_directory_entries: 1_048_576,
            max_scanned_bytes: 8 * 1024 * 1024 * 1024,
            max_source_bytes: 16 * 1024 * 1024,
            max_include_files: 4_096,
            max_include_bytes: 256 * 1024 * 1024,
            max_include_directives: 16_384,
            max_include_depth: 256,
            max_package_lookups: 256,
            max_package_unit_candidates: 1_024,
            max_package_catalogue_entries: 524_288,
            max_package_catalogues: 64,
            max_resolution_warnings: 256,
            max_compile_time_retained_bytes: 1_048_576,
            max_compile_time_work: 1_000_000,
            max_compile_time_byte_work: 16_777_216,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Unit,
    Include,
    PackageDescriptor,
}

#[derive(Debug, Clone, Copy)]
pub struct DirectoryRequest<'a> {
    pub directory: &'a Path,
    pub entry: &'a ProjectPathEntry,
    pub read_policy: &'a ReadPolicy,
}

#[derive(Debug, Clone, Copy)]
pub struct SourceRequest<'a> {
    pub path: &'a Path,
    pub entry: &'a ProjectPathEntry,
    pub read_policy: &'a ReadPolicy,
    pub legacy_route: Option<&'a LegacyRoute>,
    pub kind: SourceKind,
    pub max_bytes: usize,
}

pub trait SourceStore {
    /// Compile-time capability: defaults never invoke unbounded legacy methods.
    fn list_directory_admitted(
        &mut self,
        _request: DirectoryRequest<'_>,
        _budget: &mut crate::compile_time::CompileTimeBudget<'_>,
        _cancel: &dyn CancellationToken,
    ) -> Result<crate::compile_time::AdmittedOwned<DirectoryListing>, SourceStoreError> {
        Err(SourceStoreError::AdmissionUnavailable)
    }
    fn overlay_candidates_admitted(
        &self,
        _roots: &[PathBuf],
        _names: &[String],
        _budget: &mut crate::compile_time::CompileTimeBudget<'_>,
        _cancel: &dyn CancellationToken,
    ) -> Result<crate::compile_time::AdmittedOwned<Vec<PathBuf>>, SourceStoreError> {
        Err(SourceStoreError::AdmissionUnavailable)
    }
    fn load_admitted(
        &mut self,
        _request: SourceRequest<'_>,
        _budget: &mut crate::compile_time::CompileTimeBudget<'_>,
        _cancel: &dyn CancellationToken,
    ) -> Result<LoadedSource, SourceStoreError> {
        Err(SourceStoreError::AdmissionUnavailable)
    }
    fn list_directory(
        &mut self,
        request: DirectoryRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<DirectoryListing, SourceStoreError>;

    fn overlay_candidates(&self, roots: &[PathBuf], names: &[String]) -> Vec<PathBuf>;

    fn load(
        &mut self,
        request: SourceRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<LoadedSource, SourceStoreError>;

    /// Declared unit name for an already loaded source, when the store knows
    /// it for exactly these bytes. `Some(None)` means the source declares no
    /// unit. `None` means unknown, so the resolver parses the source.
    fn declared_unit_name(&self, _source: &LoadedSource) -> Option<Option<String>> {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryListing {
    pub files: Vec<PathBuf>,
    pub directories: Vec<PathBuf>,
    pub stamp: Option<ProjectReadStamp>,
    pub complete: bool,
}

#[derive(Debug, Clone)]
pub struct LoadedSource {
    pub id: SourceId,
    pub path: PathBuf,
    pub bytes: Arc<[u8]>,
    /// Optional caller-decoded text.  Stateful adapters can provide text in
    /// their coordinate model while retaining raw bytes for revision hashes.
    pub decoded_text: Option<Arc<str>>,
    pub revision: SourceRevision,
    /// Opaque root-minted backing/metadata owner. Legacy sources have none.
    /// Declared last so physical fields die before their final receipt.
    pub compile_time_backing: Option<crate::compile_time::AdmittedSourceBacking>,
}

impl PartialEq for LoadedSource {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.path == other.path
            && self.bytes == other.bytes
            && self.decoded_text == other.decoded_text
            && self.revision == other.revision
    }
}
impl Eq for LoadedSource {}

impl LoadedSource {
    /// Store implementers must supply a proven payload bound and construct/read
    /// only in `build`, which runs after root admission. This is the source-only
    /// capability constructor; it is not a wrapper for an unbounded legacy read.
    pub fn admit_construction(
        budget: &mut crate::compile_time::CompileTimeBudget<'_>,
        byte_work: usize,
        retained_payload: usize,
        build: impl FnOnce() -> Result<Self, SourceStoreError>,
    ) -> Result<Self, SourceStoreError> {
        let owner =
            budget.admit_fallible(1, byte_work, retained_payload, store_admission_error, build)?;
        let backing = owner
            .into_shared(budget)
            .and_then(|v| v.into_arc(budget))
            .map_err(|r| store_admission_error(r, budget))?;
        let metadata = loaded_source_metadata_retained(&backing);
        let _scratch = budget
            .admit_owned(1, metadata, metadata, || ())
            .map_err(|r| store_admission_error(r, budget))?;
        let mut source = (**backing).clone();
        source.compile_time_backing = Some(backing);
        source.admitted_copy(budget)
    }
    pub(crate) fn admitted_copy(
        &self,
        budget: &mut crate::compile_time::CompileTimeBudget<'_>,
    ) -> Result<Self, SourceStoreError> {
        if !self
            .compile_time_backing
            .as_ref()
            .is_some_and(|b| b.is_admitted_by(budget))
        {
            return Err(SourceStoreError::AdmissionUnavailable);
        }
        let metadata = loaded_source_metadata_retained(self);
        let owner = budget
            .admit_owned(1, metadata, metadata.saturating_mul(2), || self.clone())
            .and_then(|v| v.into_shared(budget))
            .and_then(|v| v.into_arc(budget))
            .map_err(|r| store_admission_error(r, budget))?;
        let mut source = (**owner).clone();
        source.compile_time_backing = Some(owner);
        Ok(source)
    }
    /// Return the UTF-8 bytes used by parser, resolver metadata, and CFG.
    ///
    /// `bytes` remains the exact payload read from disk or supplied by an
    /// overlay.  A source store may attach decoded text when that payload uses
    /// a legacy encoding (or another decoded coordinate model); consumers use
    /// this method so directive/import ranges and parsed bytes share one
    /// validated coordinate space.  The raw payload is still retained for
    /// limits, hashes, and [`SourceRevision`] revalidation.
    pub fn analysis_bytes(&self) -> Cow<'_, [u8]> {
        self.decoded_text.as_deref().map_or_else(
            || Cow::Borrowed(self.bytes.as_ref()),
            |text| Cow::Borrowed(text.as_bytes()),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceId(String);

impl SourceId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceRevision {
    Disk {
        stamp: ProjectReadStamp,
        content_hash: u64,
        read_policy: ReadPolicy,
        path_entry: ProjectPathEntry,
    },
    Overlay {
        version: i32,
        content_hash: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceStoreError {
    AdmissionUnavailable,
    RootLimit {
        limit: &'static str,
        observed: usize,
        maximum: usize,
    },
    Cancelled,
    NotFound {
        path: PathBuf,
    },
    Unauthorized {
        path: PathBuf,
        reason: String,
    },
    NotRegularFile {
        path: PathBuf,
    },
    TooLarge {
        path: PathBuf,
        maximum: usize,
    },
    Io {
        path: PathBuf,
        message: String,
    },
    Incomplete {
        path: PathBuf,
        reason: String,
    },
}

/// Incoming store failures remain plain inputs. Only original typed include-
/// limit constructors can mint the owned variant; no naked extraction exists.
enum IncludeLoadError {
    Plain(SourceStoreError),
    Limit(AdmittedOwned<SourceStoreError>),
}
impl From<SourceStoreError> for IncludeLoadError {
    fn from(error: SourceStoreError) -> Self {
        Self::Plain(error)
    }
}
impl IncludeLoadError {
    fn error_ref(&self) -> &SourceStoreError {
        match self {
            Self::Plain(error) => error,
            Self::Limit(error) => error,
        }
    }
    fn refusal(&self) -> Option<ResolverError> {
        match self.error_ref() {
            SourceStoreError::Cancelled => Some(ResolverError::Cancelled),
            SourceStoreError::RootLimit {
                limit,
                observed,
                maximum,
            } => Some(ResolverError::LimitExceeded {
                limit,
                observed: *observed,
                maximum: *maximum,
            }),
            _ => None,
        }
    }
    #[cfg(test)]
    fn move_into<T>(
        self,
        to: &mut AdmittedOwned<T>,
        budget: &CompileTimeBudget<'_>,
        install: impl FnOnce(&mut T, SourceStoreError),
    ) -> Result<(), crate::compile_time::IncompleteReason> {
        match self {
            Self::Limit(error) => to.move_field(error, budget, install),
            Self::Plain(_) => Err(crate::compile_time::IncompleteReason::MalformedSource),
        }
    }
}
impl fmt::Debug for IncludeLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error_ref().fmt(f)
    }
}

fn include_limit_error(
    path: AdmittedOwned<PathBuf>,
    kind: &'static str,
    maximum: usize,
    status: &'static str,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<IncludeLoadError, SourceStoreError> {
    if !path.is_admitted_by(budget) {
        return Err(SourceStoreError::AdmissionUnavailable);
    }
    let digits = (usize::BITS as usize)
        .checked_add(2)
        .ok_or(SourceStoreError::AdmissionUnavailable)?
        / 3;
    let bound = 32usize
        .checked_add(digits)
        .ok_or(SourceStoreError::AdmissionUnavailable)?;
    let cancel = budget.cancel_token();
    let reason = budget.admit_fallible(1, bound, bound, store_admission_error, || {
        if cancel.is_cancelled() {
            return Err(SourceStoreError::Cancelled);
        }
        let mut writer = BoundedReportString {
            string: String::with_capacity(bound),
            bound,
        };
        fmt::write(
            &mut writer,
            format_args!("include {kind} limit ({maximum}) {status}"),
        )
        .map_err(|_| SourceStoreError::AdmissionUnavailable)?;
        if cancel.is_cancelled() {
            return Err(SourceStoreError::Cancelled);
        }
        Ok(writer.string)
    })?;
    let mut error = budget
        .admit_owned(0, 0, 0, || SourceStoreError::Incomplete {
            path: PathBuf::new(),
            reason: String::new(),
        })
        .map_err(|r| store_admission_error(r, budget))?;
    error
        .move_field(path, budget, |error, path| {
            let SourceStoreError::Incomplete { path: to, .. } = error else {
                unreachable!("private limit receiver")
            };
            *to = path;
        })
        .map_err(|r| store_admission_error(r, budget))?;
    error
        .move_field(reason, budget, |error, reason| {
            let SourceStoreError::Incomplete { reason: to, .. } = error else {
                unreachable!("private limit receiver")
            };
            *to = reason;
        })
        .map_err(|r| store_admission_error(r, budget))?;
    Ok(IncludeLoadError::Limit(error))
}

fn include_limit_path_copy(
    path: &Path,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<AdmittedOwned<PathBuf>, SourceStoreError> {
    let bytes = path.as_os_str().len();
    let work = bytes
        .checked_add(1)
        .ok_or(SourceStoreError::AdmissionUnavailable)?;
    budget
        .admit_owned(work, bytes, bytes, || path.to_path_buf())
        .map_err(|r| store_admission_error(r, budget))
}

pub(crate) fn store_admission_error(
    reason: crate::compile_time::IncompleteReason,
    budget: &crate::compile_time::CompileTimeBudget<'_>,
) -> SourceStoreError {
    match crate::compile_time::resolver_error(reason, budget) {
        ResolverError::Cancelled => SourceStoreError::Cancelled,
        ResolverError::LimitExceeded {
            limit,
            observed,
            maximum,
        } => SourceStoreError::RootLimit {
            limit,
            observed,
            maximum,
        },
        _ => SourceStoreError::AdmissionUnavailable,
    }
}

fn root_store_error(error: SourceStoreError) -> ResolverError {
    match error {
        SourceStoreError::RootLimit {
            limit,
            observed,
            maximum,
        } => ResolverError::LimitExceeded {
            limit,
            observed,
            maximum,
        },
        SourceStoreError::Cancelled => ResolverError::Cancelled,
        error => ResolverError::SourceStore(error),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolverError {
    Cancelled,
    InvalidRequest(String),
    SourceStore(SourceStoreError),
    LimitExceeded {
        limit: &'static str,
        observed: usize,
        maximum: usize,
    },
}

impl fmt::Display for ResolverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => write!(formatter, "request cancelled"),
            Self::InvalidRequest(reason) => write!(formatter, "invalid resolver request: {reason}"),
            Self::SourceStore(error) => write!(formatter, "source store error: {error:?}"),
            Self::LimitExceeded {
                limit,
                observed,
                maximum,
            } => {
                write!(
                    formatter,
                    "resolver limit {limit} exceeded: {observed} > {maximum}"
                )
            }
        }
    }
}

impl std::error::Error for ResolverError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionCandidate {
    pub path: PathBuf,
    pub declared_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution<T> {
    Found(T),
    Unavailable {
        reason: String,
    },
    Ambiguous {
        candidates: Vec<ResolutionCandidate>,
    },
    Incomplete {
        reason: String,
        candidates: Vec<ResolutionCandidate>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionOutcome<T> {
    pub result: Resolution<T>,
    pub observations: Vec<ResolutionObservation>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolutionObservation {
    Directory {
        path: PathBuf,
        entry: ProjectPathEntry,
        stamp: Option<ProjectReadStamp>,
        complete: bool,
    },
    Candidate {
        path: PathBuf,
        entry: Option<ProjectPathEntry>,
        stamp: Option<ProjectReadStamp>,
        present: bool,
    },
    Payload {
        source_id: SourceId,
        path: PathBuf,
        revision: SourceRevision,
    },
    Metadata(MetadataObservation),
    ProjectRead(ProjectReadObservation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionReport {
    pub observations: Vec<ResolutionObservation>,
    pub warnings: Vec<String>,
    pub complete: bool,
    pub incomplete_reasons: Vec<String>,
}

#[derive(Debug, Default)]
struct ReportReceipt {
    recorded_work: usize,
    recorded_byte_work: usize,
}

type ReportCredit = AdmittedOwned<ReportReceipt>;

/// Receipt slots mirror public vector entries, including unbudgeted legacy
/// entries (None). Storage dies before its paired receipts on every path.
#[derive(Debug, Default)]
struct ReportFieldOwners {
    payloads: Vec<Option<ReportCredit>>,
    capacity: Option<ReportCredit>,
    receipt_capacity: Option<ReportCredit>,
}

enum ReportAction {
    Discard,
    Replace(usize),
    Append,
}

struct BoundedReportString {
    string: String,
    bound: usize,
}
struct ReportPaths<'a>(&'a [PathBuf]);
impl fmt::Display for ReportPaths<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, path) in self.0.iter().enumerate() {
            if index != 0 {
                f.write_str(", ")?;
            }
            write!(f, "{}", path.display())?;
        }
        Ok(())
    }
}
impl fmt::Write for BoundedReportString {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if s.len() > self.bound.saturating_sub(self.string.len()) {
            return Err(fmt::Error);
        }
        self.string.push_str(s);
        Ok(())
    }
}

fn observation_action(
    values: &[ResolutionObservation],
    incoming: &mut ResolutionObservation,
) -> ReportAction {
    for (index, existing) in values.iter().enumerate() {
        match (&*incoming, existing) {
            (
                ResolutionObservation::Candidate { path, present, .. },
                ResolutionObservation::Candidate { path: old, .. },
            ) if path == old => {
                return if *present {
                    ReportAction::Replace(index)
                } else {
                    ReportAction::Discard
                };
            }
            (ResolutionObservation::Metadata(new), ResolutionObservation::Metadata(old))
                if new.path() == old.path() =>
            {
                return if matches!(old, MetadataObservation::Stat { .. })
                    && matches!(new, MetadataObservation::Payload { .. })
                {
                    ReportAction::Replace(index)
                } else {
                    ReportAction::Discard
                };
            }
            (ResolutionObservation::ProjectRead(new), ResolutionObservation::ProjectRead(old))
                if new.path == old.path =>
            {
                return ReportAction::Discard;
            }
            _ if incoming == existing => return ReportAction::Discard,
            _ => {}
        }
    }
    ReportAction::Append
}

fn report_path_payload(path: &Path, entry: Option<&ProjectPathEntry>) -> usize {
    let mut bytes = path.as_os_str().len();
    if let Some(entry) = entry {
        entry
            .visit_recovery_payload(&mut |n| {
                bytes = bytes.saturating_add(n);
                Ok(())
            })
            .expect("infallible report metadata bound");
    }
    bytes
}

fn source_error_report_bound(error: &SourceStoreError) -> usize {
    let payload = match error {
        SourceStoreError::NotFound { path }
        | SourceStoreError::NotRegularFile { path }
        | SourceStoreError::TooLarge { path, .. } => path.as_os_str().len(),
        SourceStoreError::Unauthorized { path, reason }
        | SourceStoreError::Incomplete { path, reason } => {
            path.as_os_str().len().saturating_add(reason.len())
        }
        SourceStoreError::Io { path, message } => {
            path.as_os_str().len().saturating_add(message.len())
        }
        SourceStoreError::RootLimit { limit, .. } => limit.len(),
        _ => 0,
    };
    256usize.saturating_add(payload.saturating_mul(8))
}

fn resolver_error_report_bound(error: &ResolverError) -> usize {
    match error {
        ResolverError::SourceStore(error) => source_error_report_bound(error).saturating_add(32),
        ResolverError::InvalidRequest(reason) => reason.len().saturating_add(64),
        ResolverError::LimitExceeded { limit, .. } => limit.len().saturating_add(128),
        ResolverError::Cancelled => 64,
    }
}

fn resolver_report_store_error(error: ResolverError) -> SourceStoreError {
    match error {
        ResolverError::LimitExceeded {
            limit,
            observed,
            maximum,
        } => SourceStoreError::RootLimit {
            limit,
            observed,
            maximum,
        },
        ResolverError::Cancelled => SourceStoreError::Cancelled,
        error => SourceStoreError::Incomplete {
            path: PathBuf::new(),
            reason: error.to_string(),
        },
    }
}

impl ReportFieldOwners {
    fn credits(&self) -> impl Iterator<Item = &ReportCredit> {
        self.payloads
            .iter()
            .filter_map(Option::as_ref)
            .chain(self.capacity.iter())
            .chain(self.receipt_capacity.iter())
    }

    fn record<T>(
        &mut self,
        values: &mut Vec<T>,
        budget: &mut CompileTimeBudget<'_>,
        payload: usize,
        build: impl FnOnce() -> Result<T, ResolverError>,
        action: impl FnOnce(&[T], &mut T) -> ReportAction,
    ) -> Result<(), ResolverError> {
        let count = values
            .len()
            .checked_add(1)
            .ok_or_else(|| ResolverError::InvalidRequest("report capacity overflow".into()))?;
        let reserve = |budget: &mut CompileTimeBudget<'_>, bytes| {
            budget
                .admit_owned(0, 0, bytes, ReportReceipt::default)
                .map_err(|r| crate::compile_time::resolver_error(r, budget))
        };
        // Reserve *all* prospective construction/receiving peaks before build.
        // No allocation for either vector, copied payload or formatting occurs
        // between these admissions. A refusal drops only unused invoices.
        let vector = if count > values.capacity() {
            let bytes = count
                .checked_mul(std::mem::size_of::<T>())
                .ok_or_else(|| ResolverError::InvalidRequest("report vector overflow".into()))?;
            Some(reserve(budget, bytes)?)
        } else {
            None
        };
        let slots = if count > self.payloads.capacity() {
            let bytes = count
                .checked_mul(std::mem::size_of::<Option<ReportCredit>>())
                .ok_or_else(|| {
                    ResolverError::InvalidRequest("report receipt vector overflow".into())
                })?;
            Some(reserve(budget, bytes)?)
        } else {
            None
        };
        let credit = budget
            .admit_owned(1, payload, payload, ReportReceipt::default)
            .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
        let mut value = build()?;
        let action = action(values, &mut value);
        if matches!(action, ReportAction::Discard) {
            return Ok(());
        }
        if let Some(new_credit) = slots.filter(|_| {
            matches!(action, ReportAction::Append) || self.payloads.capacity() < values.len()
        }) {
            let mut new = Vec::with_capacity(count);
            new.append(&mut self.payloads);
            let old = std::mem::replace(&mut self.payloads, new);
            drop(old); // actual old receipt-slot backing dies first
            self.receipt_capacity = Some(new_credit);
        }
        self.payloads.resize_with(values.len(), || None);
        match action {
            ReportAction::Replace(index) => {
                values[index] = value; // destroy actual old payload first
                self.payloads[index] = Some(credit);
            }
            ReportAction::Append => {
                if let Some(new_credit) = vector {
                    let mut new = Vec::with_capacity(count);
                    new.append(values);
                    let old = std::mem::replace(values, new);
                    drop(old); // old-plus-new peak; retire only dead backing
                    self.capacity = Some(new_credit);
                }
                values.push(value);
                self.payloads.push(Some(credit));
            }
            ReportAction::Discard => unreachable!(),
        }
        Ok(())
    }

    fn into_raw_floor(self) {
        let Self {
            payloads,
            capacity,
            receipt_capacity,
        } = self;
        for credit in payloads.into_iter().flatten() {
            credit.into_raw_report_floor();
        }
        if let Some(credit) = capacity {
            credit.into_raw_report_floor();
        }
        // Receipt slots were consumed and their Vec destroyed by into_iter;
        // they are not caller report storage and must refund, not become floor.
        drop(receipt_capacity);
    }
}

#[derive(Debug, Default)]
struct ReportOwners {
    observations: ReportFieldOwners,
    warnings: ReportFieldOwners,
    diagnostics: ReportFieldOwners,
}
impl ReportOwners {
    fn credits(&self) -> impl Iterator<Item = &ReportCredit> {
        self.observations
            .credits()
            .chain(self.warnings.credits())
            .chain(self.diagnostics.credits())
    }
    fn record_work(&mut self, budget: &CompileTimeBudget<'_>) {
        let snapshot = budget.snapshot();
        for field in [
            &mut self.observations,
            &mut self.warnings,
            &mut self.diagnostics,
        ] {
            for credit in field
                .payloads
                .iter_mut()
                .filter_map(Option::as_mut)
                .chain(field.capacity.iter_mut())
                .chain(field.receipt_capacity.iter_mut())
            {
                if credit.is_admitted_by(budget) {
                    credit.value_mut().recorded_work = snapshot.work;
                    credit.value_mut().recorded_byte_work = snapshot.byte_work;
                }
            }
        }
    }
    fn into_raw_floor(self) {
        self.observations.into_raw_floor();
        self.warnings.into_raw_floor();
        self.diagnostics.into_raw_floor();
    }
}

/// Typed report handoff: immutable borrowed access cannot detach plain fields
/// from their independently minted, possibly multi-root report receipts.
#[derive(Debug)]
pub struct AdmittedResolutionReport {
    report: ResolutionReport,
    owners: ReportOwners,
}
impl AdmittedResolutionReport {
    pub fn report(&self) -> &ResolutionReport {
        &self.report
    }
}

#[derive(Debug)]
pub struct AdmittedReportObservations {
    observations: Vec<ResolutionObservation>,
    _owners: ReportFieldOwners,
}

#[derive(Debug)]
struct CapabilityReportField<T> {
    values: Vec<T>,
    owners: ReportFieldOwners,
}

#[derive(Debug)]
struct CapabilityReportDrain<T> {
    values: std::vec::IntoIter<T>,
    payloads: std::vec::IntoIter<Option<ReportCredit>>,
    _capacity: Option<ReportCredit>,
    _receipt_capacity: Option<ReportCredit>,
}

/// Independently admitted returned report buffers. Borrowing cannot detach
/// their payload from its receipts; consuming transfer is same-root only.
#[derive(Debug)]
pub struct AdmittedCapabilityReports {
    observations: AdmittedOwned<CapabilityReportField<ResolutionObservation>>,
    warnings: AdmittedOwned<CapabilityReportField<String>>,
}

#[derive(Debug)]
pub struct AdmittedCapabilityOutcome<T> {
    result: AdmittedCapabilityResult<T>,
    reports: AdmittedCapabilityReports,
}

/// Borrow-only callback result. Migrated reason/candidate payloads cannot be
/// detached from their original construction receipts by splitting the outcome.
#[derive(Debug)]
pub struct AdmittedCapabilityResult<T> {
    storage: CapabilityResultStorage<T>,
}
#[derive(Debug)]
enum CapabilityResultStorage<T> {
    Unmigrated(Resolution<T>),
    Reason(AdmittedOwned<Resolution<T>>),
    Candidates(AdmittedOwned<Resolution<T>>),
}
impl<T> From<Resolution<T>> for AdmittedCapabilityResult<T> {
    fn from(value: Resolution<T>) -> Self {
        Self {
            storage: CapabilityResultStorage::Unmigrated(value),
        }
    }
}
impl<T> AdmittedCapabilityResult<T> {
    pub fn result(&self) -> &Resolution<T> {
        match &self.storage {
            CapabilityResultStorage::Unmigrated(value) => value,
            CapabilityResultStorage::Reason(value) | CapabilityResultStorage::Candidates(value) => {
                value
            }
        }
    }
    pub(crate) fn owns_reason(&self) -> bool {
        matches!(self.storage, CapabilityResultStorage::Reason(_))
            || matches!(&self.storage,CapabilityResultStorage::Candidates(value) if matches!(&**value,Resolution::Incomplete {..}))
    }
    pub(crate) fn owns_candidates(&self) -> bool {
        matches!(self.storage, CapabilityResultStorage::Candidates(_))
    }
    pub(crate) fn into_unmigrated(self) -> Result<Resolution<T>, ResolverError> {
        match self.storage {
            CapabilityResultStorage::Unmigrated(value) => Ok(value),
            CapabilityResultStorage::Reason(_) | CapabilityResultStorage::Candidates(_) => {
                Err(ResolverError::InvalidRequest(
                    "owned result fields require consuming field transfer".into(),
                ))
            }
        }
    }
    pub(crate) fn move_reason_into<U>(
        self,
        destination: &mut AdmittedOwned<U>,
        budget: &CompileTimeBudget<'_>,
        install: impl FnOnce(&mut U, Resolution<T>),
    ) -> Result<(), ResolverError> {
        if !self.owns_reason() {
            return Err(ResolverError::InvalidRequest(
                "result has no original reason receipt".into(),
            ));
        }
        self.move_fields_into(destination, budget, install)
    }
    pub(crate) fn move_fields_into<U>(
        self,
        destination: &mut AdmittedOwned<U>,
        budget: &CompileTimeBudget<'_>,
        install: impl FnOnce(&mut U, Resolution<T>),
    ) -> Result<(), ResolverError> {
        match self.storage {
            CapabilityResultStorage::Reason(value) | CapabilityResultStorage::Candidates(value) => {
                destination
                    .move_field(value, budget, install)
                    .map_err(|r| crate::compile_time::resolver_error(r, budget))
            }
            CapabilityResultStorage::Unmigrated(_) => Err(ResolverError::InvalidRequest(
                "result has no original owned field receipt".into(),
            )),
        }
    }
    pub(crate) fn reason(
        reason: AdmittedOwned<String>,
        incomplete: bool,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<Self, ResolverError> {
        Self::reason_with_candidates(reason, incomplete, Vec::new(), budget)
    }
    fn candidates(
        candidates: AdmittedOwned<Vec<ResolutionCandidate>>,
        reason: Option<AdmittedOwned<String>>,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<Self, ResolverError> {
        let incomplete = reason.is_some();
        let mut value = candidates
            .map_value(budget, |candidates| {
                if incomplete {
                    Resolution::Incomplete {
                        reason: String::new(),
                        candidates,
                    }
                } else {
                    Resolution::Ambiguous { candidates }
                }
            })
            .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
        if let Some(reason) = reason {
            value
                .move_field(reason, budget, |result, reason| {
                    let Resolution::Incomplete { reason: field, .. } = result else {
                        unreachable!()
                    };
                    *field = reason;
                })
                .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
        }
        Ok(Self {
            storage: CapabilityResultStorage::Candidates(value),
        })
    }
    fn reason_with_candidates(
        reason: AdmittedOwned<String>,
        incomplete: bool,
        candidates: Vec<ResolutionCandidate>,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<Self, ResolverError> {
        let value = reason
            .map_value(budget, |reason| {
                if incomplete {
                    Resolution::Incomplete { reason, candidates }
                } else {
                    Resolution::Unavailable { reason }
                }
            })
            .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
        Ok(Self {
            storage: CapabilityResultStorage::Reason(value),
        })
    }
}
impl<T> AdmittedCapabilityOutcome<T> {
    pub fn from_reason(
        reason: AdmittedOwned<String>,
        incomplete: bool,
        reports: AdmittedCapabilityReports,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<Self, ResolverError> {
        Ok(Self {
            result: AdmittedCapabilityResult::reason(reason, incomplete, budget)?,
            reports,
        })
    }
    pub fn new(result: Resolution<T>, reports: AdmittedCapabilityReports) -> Self {
        Self {
            result: result.into(),
            reports,
        }
    }
    /// Bounded adapters copy borrowed failure reasons before construction.
    /// Production resolvers instead hand off the original minted reason owner.
    pub fn copy_bounded_result(
        result: Resolution<T>,
        reports: AdmittedCapabilityReports,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<Self, ResolverError> {
        let reason = match &result {
            Resolution::Unavailable { reason }
            | Resolution::Incomplete {
                reason,
                candidates: _,
            } => Some(reason),
            _ => None,
        };
        let Some(reason) = reason else {
            return Ok(Self::new(result, reports));
        };
        if matches!(&result,Resolution::Incomplete {candidates,..} if !candidates.is_empty()) {
            return Ok(Self::new(result, reports)); // candidate ownership is still unmigrated
        }
        let owner = budget
            .admit_owned(1, reason.len(), reason.len(), || reason.clone())
            .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
        let incomplete = matches!(result, Resolution::Incomplete { .. });
        drop(result);
        Ok(Self {
            result: AdmittedCapabilityResult::reason(owner, incomplete, budget)?,
            reports,
        })
    }
    pub fn result(&self) -> &Resolution<T> {
        self.result.result()
    }
    pub fn reports(&self) -> &AdmittedCapabilityReports {
        &self.reports
    }
    pub fn into_parts(self) -> (AdmittedCapabilityResult<T>, AdmittedCapabilityReports) {
        (self.result, self.reports)
    }
}

impl AdmittedCapabilityReports {
    /// Clone each legitimately required returned payload only after its own
    /// invoice and receiving peaks. Persistent reports retain separate owners.
    pub fn copy(
        observations: &[ResolutionObservation],
        warnings: &[String],
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<Self, ResolverError> {
        Self::copy_with(observations, warnings, budget, Clone::clone, Clone::clone)
    }

    fn copy_with(
        observations: &[ResolutionObservation],
        warnings: &[String],
        budget: &mut CompileTimeBudget<'_>,
        mut observation: impl FnMut(&ResolutionObservation) -> ResolutionObservation,
        mut warning: impl FnMut(&String) -> String,
    ) -> Result<Self, ResolverError> {
        let mut obs = budget
            .admit_owned(0, 0, 0, || CapabilityReportField {
                values: Vec::new(),
                owners: ReportFieldOwners::default(),
            })
            .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
        let mut warns = budget
            .admit_owned(0, 0, 0, || CapabilityReportField {
                values: Vec::new(),
                owners: ReportFieldOwners::default(),
            })
            .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
        for input in observations {
            let payload = crate::compile_time::observation_payload(input)?
                - std::mem::size_of::<ResolutionObservation>();
            let field = obs.value_mut();
            field.owners.record(
                &mut field.values,
                budget,
                payload,
                || Ok(observation(input)),
                |_, _| ReportAction::Append,
            )?;
        }
        for input in warnings {
            let field = warns.value_mut();
            field.owners.record(
                &mut field.values,
                budget,
                input.len(),
                || Ok(warning(input)),
                |_, _| ReportAction::Append,
            )?;
        }
        Ok(Self {
            observations: obs,
            warnings: warns,
        })
    }

    pub fn observations(&self) -> &[ResolutionObservation] {
        &self.observations.values
    }
    pub fn warnings(&self) -> &[String] {
        &self.warnings.values
    }

    pub(crate) fn consume_into(
        self,
        observations: &mut AdmittedOwned<Vec<ResolutionObservation>>,
        warnings: &mut AdmittedOwned<Vec<String>>,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<(), ResolverError> {
        // Validate both fields before any destination mutation. No callback
        // result/provider/cache receipt is treated as report storage.
        if !observations.is_admitted_by(budget)
            || !warnings.is_admitted_by(budget)
            || !self.observations.is_admitted_by(budget)
            || !self.warnings.is_admitted_by(budget)
        {
            return Err(ResolverError::InvalidRequest(
                "foreign-root capability report".into(),
            ));
        }
        consume_report_field(self.observations, observations, budget)?;
        consume_report_field(self.warnings, warnings, budget)
    }
}

fn consume_report_field<T: PartialEq>(
    mut source: AdmittedOwned<CapabilityReportField<T>>,
    target: &mut AdmittedOwned<Vec<T>>,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<(), ResolverError> {
    let refusal = |r, b: &CompileTimeBudget<'_>| crate::compile_time::resolver_error(r, b);
    if !source.is_admitted_by(budget)
        || !target.is_admitted_by(budget)
        || source.owners.credits().any(|c| !c.is_admitted_by(budget))
    {
        return Err(ResolverError::InvalidRequest(
            "foreign-root report field".into(),
        ));
    }
    let count = source.values.len();
    let work = count.saturating_mul(target.len().saturating_add(count).saturating_add(1));
    budget.charge(work, 0, 0).map_err(|r| refusal(r, budget))?;
    if count == 0 {
        return Ok(());
    }
    let unique = source
        .values
        .iter()
        .enumerate()
        .filter(|(i, value)| !target.contains(value) && !source.values[..*i].contains(value))
        .count();
    if target.is_empty() && target.capacity() == 0 && unique == count {
        // Fold only named report payload/backing receipts into this actual
        // source owner, then hand off its whole Vec without reallocating it.
        while let Some(credit) = source.value_mut().owners.payloads.pop() {
            let credit = credit.expect("returned report entries are admitted");
            source
                .move_field(credit, budget, |_, _| ())
                .map_err(|r| refusal(r, budget))?;
        }
        let slots = std::mem::take(&mut source.value_mut().owners.payloads);
        let slot_credit = source.value_mut().owners.receipt_capacity.take();
        drop(slots);
        drop(slot_credit);
        if let Some(credit) = source.value_mut().owners.capacity.take() {
            source
                .move_field(credit, budget, |_, _| ())
                .map_err(|r| refusal(r, budget))?;
        }
        target
            .move_field(source, budget, |values, field| *values = field.values)
            .map_err(|r| refusal(r, budget))?;
        return Ok(());
    }
    let needed = target
        .len()
        .checked_add(unique)
        .ok_or_else(|| ResolverError::InvalidRequest("report receiving count overflow".into()))?;
    if needed > target.capacity() {
        let old = target
            .capacity()
            .checked_mul(std::mem::size_of::<T>())
            .ok_or_else(|| ResolverError::InvalidRequest("report old backing overflow".into()))?;
        let new = needed
            .checked_mul(std::mem::size_of::<T>())
            .ok_or_else(|| {
                ResolverError::InvalidRequest("report receiving backing overflow".into())
            })?;
        target
            .grow(budget, 0, 0, new, |values| {
                let mut receiving = Vec::with_capacity(needed);
                receiving.append(values);
                let old = std::mem::replace(values, receiving);
                drop(old);
            })
            .map_err(|r| refusal(r, budget))?;
        target.retire_storage(old).map_err(|r| refusal(r, budget))?;
    }
    let mut drain = source
        .map_value(budget, |field| CapabilityReportDrain {
            values: field.values.into_iter(),
            payloads: field.owners.payloads.into_iter(),
            _capacity: field.owners.capacity,
            _receipt_capacity: field.owners.receipt_capacity,
        })
        .map_err(|r| refusal(r, budget))?;
    while let Some(value) = drain.value_mut().values.next() {
        let credit = drain
            .value_mut()
            .payloads
            .next()
            .flatten()
            .expect("named returned report payload");
        if target.contains(&value) {
            drop(value);
            drop(credit);
            continue;
        }
        // The consuming transform moves the actual value together with its
        // exact named receipt, not an invoice inferred from payload length.
        let entry = credit
            .map_value(budget, |_| value)
            .map_err(|r| refusal(r, budget))?;
        target
            .move_field(entry, budget, |values, value| values.push(value))
            .map_err(|r| refusal(r, budget))?;
    }
    Ok(()) // drain destroys actual old buffers before their capacity receipts
}
impl AdmittedReportObservations {
    pub fn observations(&self) -> &[ResolutionObservation] {
        &self.observations
    }
}
impl AdmittedOwned<AdmittedResolutionReport> {
    /// Move the actual observation buffer/receipts; destroy the other report
    /// fields before refunding their named owners. Destination control precedes
    /// the transfer, and the envelope's own minting identity is checked.
    pub fn into_report_observations(
        self,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<AdmittedOwned<AdmittedReportObservations>, ResolverError> {
        self.map_value(budget, |report| {
            let ResolutionReport {
                observations,
                warnings,
                incomplete_reasons,
                ..
            } = report.report;
            drop(warnings);
            drop(incomplete_reasons);
            let ReportOwners {
                observations: owners,
                warnings,
                diagnostics,
            } = report.owners;
            drop(warnings);
            drop(diagnostics);
            AdmittedReportObservations {
                observations,
                _owners: owners,
            }
        })
        .map_err(|r| crate::compile_time::resolver_error(r, budget))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitResolveRequest<'a> {
    pub requested_name: &'a str,
    pub importer_path: &'a Path,
    pub legacy_route: Option<&'a LegacyRoute>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LegacyRoute {
    pub source_path: PathBuf,
    pub sibling_directory: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSite {
    pub byte_range: Range<usize>,
    pub requested_name: String,
    pub section: ImportSection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncludeResolveRequest<'a> {
    pub including_path: &'a Path,
    pub byte_range: Range<usize>,
    pub requested_name: &'a str,
    pub legacy_route: Option<&'a LegacyRoute>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportSection {
    Module,
    Interface,
    Implementation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedUnit {
    pub requested_name: String,
    pub declared_name: String,
    pub source: LoadedSource,
}

pub type ResolvedSource = LoadedSource;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolutionTarget<T> {
    Found(T),
    Unavailable,
    Ambiguous,
    Incomplete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedImport {
    pub importer_source_id: SourceId,
    pub site: ImportSite,
    pub target: ResolutionTarget<SourceId>,
    pub authorized_qualifiers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedInclude {
    pub including_source_id: SourceId,
    pub byte_range: Range<usize>,
    pub requested_name: String,
    pub target: ResolutionTarget<SourceId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedImports {
    pub bindings: Vec<ResolvedImport>,
    pub dependencies: Vec<ResolvedUnit>,
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProject {
    pub root: ResolvedUnit,
    pub units: Vec<ResolvedUnit>,
    pub imports: Vec<ResolvedImport>,
    pub includes: Vec<ResolvedInclude>,
    pub include_sources: Vec<LoadedSource>,
    pub complete: bool,
    pub report: ResolutionReport,
    pub conditional_contexts: HashMap<SourceId, pascal_project::ConditionalContext>,
}

#[path = "resolver_store.rs"]
mod store;
pub use store::{FilesystemSourceStore, OverlaySource};

/// Resolver-owned caches and observations are request-local. Callers may
/// explicitly share bounded package catalogues with an operation-scoped cache.
pub struct UnitResolver<S> {
    context: ProjectContext,
    workspace_roots: Vec<PathBuf>,
    store: S,
    limits: ResolverLimits,
    report: ResolutionReport,
    // Declared after report: actual buffers die before paired credits refund.
    report_owners: ReportOwners,
    loaded: HashMap<SourceId, LoadedSource>,
    directories: HashMap<DirectoryCacheKey, DirectoryListing>,
    package_catalogues: HashMap<PathBuf, Arc<Catalogue>>,
    session_cache: Option<ResolverSessionCache>,
    unit_cache: HashMap<UnitCacheKey, ResolvedUnit>,
    legacy_routes: HashMap<SourceId, LegacyRoute>,
    project_unit_ids: Option<HashSet<SourceId>>,
    include_files: usize,
    include_bytes: usize,
    include_directives: usize,
    package_lookups: usize,
    package_catalogue_count: usize,
    scanned_entries: usize,
    scanned_bytes: usize,
    project_includes: Vec<ResolvedInclude>,
    include_seen: HashSet<SourceId>,
    source_metadata: HashMap<SourceId, CachedSourceMetadata>,
    stop_when_incomplete: bool,
    compile_time_route: Option<LegacyRoute>,
    compile_time_root_path: Option<PathBuf>,
    compile_time_accounting: crate::compile_time::BudgetSnapshot,
    compile_time_store: Option<StoreRoot>,
    compile_time_caches: Vec<StoreRoot>,
    callback_reports: Option<CallbackReports>,
    callback_include_reason: Option<AdmittedOwned<String>>,
    callback_unit_reason: Option<AdmittedOwned<String>>,
    callback_unit_candidates: Option<AdmittedOwned<Vec<ResolutionCandidate>>>,
    owning_unit_candidates: bool,
    owning_unit_reason: bool,
    owning_include_callback: bool,
    owning_unit_callback: bool,
    callback_unit_names: Option<OwnedUnitNames>,
    callback_provider: Option<AdmittedOwned<crate::compile_time::ProviderSource>>,
}

struct OwnedUnitNames {
    requested: AdmittedOwned<String>,
    declared: AdmittedOwned<String>,
}

enum CallbackReports {
    Requested,
    Built(AdmittedCapabilityReports),
}
struct CallbackReportCall<'a, S: SourceStore> {
    resolver: &'a mut UnitResolver<S>,
    previous: Option<CallbackReports>,
    previous_reason: Option<AdmittedOwned<String>>,
    previous_unit_reason: Option<AdmittedOwned<String>>,
    previous_candidates: Option<AdmittedOwned<Vec<ResolutionCandidate>>>,
    previous_candidates_mode: bool,
    previous_unit_reason_mode: bool,
    previous_include: bool,
    previous_unit: bool,
    previous_names: Option<OwnedUnitNames>,
    previous_provider: Option<AdmittedOwned<crate::compile_time::ProviderSource>>,
}
impl<S: SourceStore> std::ops::Deref for CallbackReportCall<'_, S> {
    type Target = UnitResolver<S>;
    fn deref(&self) -> &Self::Target {
        self.resolver
    }
}
impl<S: SourceStore> std::ops::DerefMut for CallbackReportCall<'_, S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.resolver
    }
}
impl<S: SourceStore> Drop for CallbackReportCall<'_, S> {
    fn drop(&mut self) {
        self.resolver.callback_reports = self.previous.take();
        self.resolver.callback_include_reason = self.previous_reason.take();
        self.resolver.callback_unit_reason = self.previous_unit_reason.take();
        self.resolver.callback_unit_candidates = self.previous_candidates.take();
        self.resolver.owning_unit_candidates = self.previous_candidates_mode;
        self.resolver.owning_unit_reason = self.previous_unit_reason_mode;
        self.resolver.owning_include_callback = self.previous_include;
        self.resolver.owning_unit_callback = self.previous_unit;
        self.resolver.callback_unit_names = self.previous_names.take();
        self.resolver.callback_provider = self.previous_provider.take();
    }
}
impl<S: SourceStore> CallbackReportCall<'_, S> {
    fn finish<T>(
        self,
        result: Resolution<T>,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<AdmittedCapabilityOutcome<T>, ResolverError> {
        let Some(CallbackReports::Built(reports)) = self.resolver.callback_reports.take() else {
            return Err(ResolverError::InvalidRequest(
                "callback report capability was not constructed".into(),
            ));
        };
        if let Some(reason) = self.resolver.callback_include_reason.take() {
            let incomplete = match &result {
                Resolution::Unavailable { reason } if reason.is_empty() => false,
                Resolution::Incomplete { reason, candidates }
                    if reason.is_empty() && candidates.is_empty() =>
                {
                    true
                }
                _ => {
                    return Err(ResolverError::InvalidRequest(
                        "include reason constructor/handoff mismatch".into(),
                    ));
                }
            };
            drop(result);
            return Ok(AdmittedCapabilityOutcome {
                result: AdmittedCapabilityResult::reason(reason, incomplete, budget)?,
                reports,
            });
        }
        if self.resolver.owning_unit_candidates
            && matches!(
                &result,
                Resolution::Ambiguous { .. } | Resolution::Incomplete { .. }
            )
        {
            match &result {
                Resolution::Ambiguous { candidates } if candidates.is_empty() => {}
                Resolution::Incomplete { reason, candidates }
                    if reason.is_empty() && candidates.is_empty() => {}
                _ => {
                    return Err(ResolverError::InvalidRequest(
                        "unit candidate constructor/handoff mismatch".into(),
                    ));
                }
            }
            let candidates = match self.resolver.callback_unit_candidates.take() {
                Some(candidates) => candidates,
                None => candidate_vector(0, budget)?,
            };
            let reason = self.resolver.callback_unit_reason.take();
            if matches!(&result, Resolution::Incomplete { .. }) != reason.is_some() {
                return Err(ResolverError::InvalidRequest(
                    "unit candidate reason handoff mismatch".into(),
                ));
            }
            drop(result);
            return Ok(AdmittedCapabilityOutcome {
                result: AdmittedCapabilityResult::candidates(candidates, reason, budget)?,
                reports,
            });
        }
        if let Some(reason) = self.resolver.callback_unit_reason.take() {
            let (incomplete, candidates) = match result {
                Resolution::Unavailable { reason } if reason.is_empty() => (false, Vec::new()),
                Resolution::Incomplete { reason, candidates } if reason.is_empty() => {
                    (true, candidates)
                }
                _ => {
                    return Err(ResolverError::InvalidRequest(
                        "unit reason constructor/handoff mismatch".into(),
                    ));
                }
            };
            return Ok(AdmittedCapabilityOutcome {
                result: AdmittedCapabilityResult::reason_with_candidates(
                    reason, incomplete, candidates, budget,
                )?,
                reports,
            });
        }
        Ok(AdmittedCapabilityOutcome::new(result, reports))
    }
}

#[derive(Default)]
struct StoreCache {
    loaded: HashMap<OwnedLoadedKey, LoadedSource>,
    directories: HashMap<OwnedDirectoryKey, AdmittedShared<DirectoryListing>>,
    loaded_capacity: Option<AdmittedOwned<()>>,
    include_seen: HashSet<OwnedLoadedKey>,
    include_seen_capacity: Option<AdmittedOwned<()>>,
    directory_capacity: Option<AdmittedOwned<()>>,
    recorded_work: usize,
    recorded_byte_work: usize,
}

/// Original cloned key storage and its receipt enter/leave the map together.
struct OwnedLoadedKey(AdmittedOwned<SourceId>);

// Real receiving allocations precede their receipts in drop order. They are
// empty until commit, never snapshots/clones of prior cache contents.
struct PreparedLoadedGrowth {
    receiving: HashMap<OwnedLoadedKey, LoadedSource>,
    capacity: AdmittedOwned<()>,
}
struct PreparedSeenGrowth {
    receiving: HashSet<OwnedLoadedKey>,
    capacity: AdmittedOwned<()>,
}
struct PreparedLoaded {
    key: OwnedLoadedKey,
    source: LoadedSource,
    growth: Option<PreparedLoadedGrowth>,
}
struct PreparedSeen {
    key: OwnedLoadedKey,
    growth: Option<PreparedSeenGrowth>,
}
struct PreparedSourceLoad {
    loaded: LoadedSource,
    cached: Option<(OwnedLoadedKey, LoadedSource)>,
}
impl std::hash::Hash for OwnedLoadedKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}
impl PartialEq for OwnedLoadedKey {
    fn eq(&self, other: &Self) -> bool {
        *self.0 == *other.0
    }
}
impl Eq for OwnedLoadedKey {}
impl std::borrow::Borrow<SourceId> for OwnedLoadedKey {
    fn borrow(&self) -> &SourceId {
        &self.0
    }
}

fn loaded_cache_key(
    id: &SourceId,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<OwnedLoadedKey, SourceStoreError> {
    budget
        .admit_owned(1, id.as_str().len(), id.as_str().len(), || id.clone())
        .map(OwnedLoadedKey)
        .map_err(|r| store_admission_error(r, budget))
}

/// A key is moved into the map with its original construction receipt. There
/// is no detached cache invoice for a cloned key or a temporary caller listing.
struct OwnedDirectoryKey(AdmittedOwned<DirectoryCacheKey>);
impl std::hash::Hash for OwnedDirectoryKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}
impl PartialEq for OwnedDirectoryKey {
    fn eq(&self, other: &Self) -> bool {
        *self.0 == *other.0
    }
}
impl Eq for OwnedDirectoryKey {}
impl std::borrow::Borrow<DirectoryCacheKey> for OwnedDirectoryKey {
    fn borrow(&self) -> &DirectoryCacheKey {
        &self.0
    }
}

enum DirectoryAccess {
    Legacy(DirectoryListing),
    Admitted(AdmittedShared<DirectoryListing>),
}
impl std::ops::Deref for DirectoryAccess {
    type Target = DirectoryListing;
    fn deref(&self) -> &DirectoryListing {
        match self {
            Self::Legacy(listing) => listing,
            Self::Admitted(listing) => listing,
        }
    }
}

impl StoreCache {
    fn prepare_loaded_pair(
        &self,
        key: OwnedLoadedKey,
        source: LoadedSource,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<PreparedLoaded, SourceStoreError> {
        if !key.0.is_admitted_by(budget)
            || !source
                .compile_time_backing
                .as_ref()
                .is_some_and(|owner| owner.is_admitted_by(budget))
            || self
                .loaded_capacity
                .as_ref()
                .is_some_and(|owner| !owner.is_admitted_by(budget))
        {
            return Err(SourceStoreError::AdmissionUnavailable);
        }
        let overflow = || SourceStoreError::AdmissionUnavailable;
        // Hashing for contains/remove/insert is input-sized, even on replacement.
        let hash_bytes = key.0.as_str().len().checked_mul(3).ok_or_else(overflow)?;
        budget
            .charge(3, hash_bytes, 0)
            .map_err(|r| store_admission_error(r, budget))?;
        let replace = self.loaded.contains_key(&*key.0);
        let growth = if !replace && self.loaded.len() == self.loaded.capacity() {
            let count = self.loaded.len().checked_add(1).ok_or_else(overflow)?;
            let buckets = count
                .checked_next_power_of_two()
                .and_then(|n| n.checked_mul(2))
                .map(|n| n.max(4))
                .ok_or_else(overflow)?;
            let bytes = buckets
                .checked_mul(std::mem::size_of::<(OwnedLoadedKey, LoadedSource)>() + 1)
                .and_then(|n| n.checked_add(16))
                .ok_or_else(overflow)?;
            // Admit scalar sizing visits before walking old keys; rehash work
            // and receiving allocation get their own admission afterwards.
            budget
                .charge(self.loaded.len(), 0, 0)
                .map_err(|r| store_admission_error(r, budget))?;
            let mut rehash_bytes = 0usize;
            for old in self.loaded.keys() {
                if budget.cancel_token().is_cancelled() {
                    return Err(SourceStoreError::Cancelled);
                }
                rehash_bytes = rehash_bytes
                    .checked_add(old.0.as_str().len())
                    .ok_or_else(overflow)?;
            }
            let capacity = budget
                .admit_owned(
                    self.loaded.len().checked_add(1).ok_or_else(overflow)?,
                    rehash_bytes,
                    bytes,
                    || (),
                )
                .map_err(|r| store_admission_error(r, budget))?;
            // All refusal/cancellation callbacks precede the destructive move.
            // No store reads or recursive calls occur under this cache lock.
            Some(PreparedLoadedGrowth {
                receiving: HashMap::with_capacity(count),
                capacity,
            })
        } else {
            None
        };
        Ok(PreparedLoaded {
            key,
            source,
            growth,
        })
    }

    fn commit_loaded(&mut self, prepared: PreparedLoaded) {
        let PreparedLoaded {
            key,
            source,
            growth,
        } = prepared;
        if let Some(PreparedLoadedGrowth {
            mut receiving,
            capacity,
        }) = growth
        {
            receiving.extend(self.loaded.drain());
            drop(std::mem::replace(&mut self.loaded, receiving));
            self.loaded_capacity = Some(capacity);
        }
        // insert alone retains an equal old key. Destroy the actual old pair
        // before dropping its owners, then consume the prepared new pair.
        drop(self.loaded.remove_entry(&*key.0));
        self.loaded.insert(key, source);
    }

    fn commit_seen(&mut self, prepared: PreparedSeen) {
        let PreparedSeen { key, growth } = prepared;
        if let Some(PreparedSeenGrowth {
            mut receiving,
            capacity,
        }) = growth
        {
            receiving.extend(self.include_seen.drain());
            drop(std::mem::replace(&mut self.include_seen, receiving));
            self.include_seen_capacity = Some(capacity);
        }
        self.include_seen.insert(key);
    }

    fn insert_directory(
        &mut self,
        key: OwnedDirectoryKey,
        listing: AdmittedShared<DirectoryListing>,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<(), ResolverError> {
        if !key.0.is_admitted_by(budget) || !listing.is_admitted_by(budget) {
            return Err(ResolverError::InvalidRequest(
                "foreign-root directory cache entry".into(),
            ));
        }
        let replace = self.directories.contains_key(&*key.0);
        if !replace && self.directories.len() == self.directories.capacity() {
            let count = self.directories.len().checked_add(1).ok_or_else(|| {
                ResolverError::InvalidRequest("directory map count overflow".into())
            })?;
            // A conservative hash-table backing bound includes bucket payload,
            // control bytes and alignment slack. It is minted for this actual
            // allocation, retained until that allocation is replaced/dropped.
            let buckets = count
                .checked_next_power_of_two()
                .and_then(|n| n.checked_mul(2))
                .map(|n| n.max(4))
                .ok_or_else(|| {
                    ResolverError::InvalidRequest("directory map bucket overflow".into())
                })?;
            let bytes = buckets
                .checked_mul(
                    std::mem::size_of::<(OwnedDirectoryKey, AdmittedShared<DirectoryListing>)>()
                        + 1,
                )
                .and_then(|n| n.checked_add(16))
                .ok_or_else(|| {
                    ResolverError::InvalidRequest("directory map backing overflow".into())
                })?;
            let capacity = budget
                .admit_owned(1, 0, bytes, || ())
                .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
            let mut receiving = HashMap::with_capacity(count);
            receiving.extend(self.directories.drain());
            let old = std::mem::replace(&mut self.directories, receiving);
            drop(old);
            self.directory_capacity = Some(capacity);
        }
        // HashMap::insert retains an equal old key. Remove the old pair so the
        // new actual key and receipt move together, and old storage dies first.
        let old = self.directories.remove_entry(&*key.0);
        drop(old);
        self.directories.insert(key, listing);
        Ok(())
    }
}

impl AdmittedOwned<StoreCache> {
    fn prepare_seen(
        &self,
        key: OwnedLoadedKey,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<Option<PreparedSeen>, SourceStoreError> {
        if !self.is_admitted_by(budget) || !key.0.is_admitted_by(budget) {
            return Err(SourceStoreError::AdmissionUnavailable);
        }
        let overflow = || SourceStoreError::AdmissionUnavailable;
        budget
            .charge(
                2,
                key.0.as_str().len().checked_mul(2).ok_or_else(overflow)?,
                0,
            )
            .map_err(|r| store_admission_error(r, budget))?;
        if self.include_seen.contains(&*key.0) {
            return Ok(None);
        }
        let cache = &**self;
        let growth = if cache.include_seen.len() == cache.include_seen.capacity() {
            let count = cache
                .include_seen
                .len()
                .checked_add(1)
                .ok_or_else(overflow)?;
            let buckets = count
                .checked_next_power_of_two()
                .and_then(|n| n.checked_mul(2))
                .map(|n| n.max(4))
                .ok_or_else(overflow)?;
            let bytes = buckets
                .checked_mul(std::mem::size_of::<OwnedLoadedKey>() + 1)
                .and_then(|n| n.checked_add(16))
                .ok_or_else(overflow)?;
            budget
                .charge(cache.include_seen.len(), 0, 0)
                .map_err(|r| store_admission_error(r, budget))?;
            let mut rehash = 0usize;
            for key in &cache.include_seen {
                if budget.cancel_token().is_cancelled() {
                    return Err(SourceStoreError::Cancelled);
                }
                rehash = rehash
                    .checked_add(key.0.as_str().len())
                    .ok_or_else(overflow)?;
            }
            let capacity = budget
                .admit_owned(count, rehash, bytes, || ())
                .map_err(|r| store_admission_error(r, budget))?;
            Some(PreparedSeenGrowth {
                receiving: HashSet::with_capacity(count),
                capacity,
            })
        } else {
            None
        };
        Ok(Some(PreparedSeen { key, growth }))
    }

    #[cfg(test)]
    fn insert_seen(
        &mut self,
        key: OwnedLoadedKey,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<bool, SourceStoreError> {
        let prepared = self.prepare_seen(key, budget)?;
        let inserted = prepared.is_some();
        if let Some(prepared) = prepared {
            self.value_mut().commit_seen(prepared);
        }
        Ok(inserted)
    }

    fn prepare_loaded(
        &self,
        key: OwnedLoadedKey,
        source: LoadedSource,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<PreparedLoaded, SourceStoreError> {
        if !self.is_admitted_by(budget) {
            return Err(SourceStoreError::AdmissionUnavailable);
        }
        self.prepare_loaded_pair(key, source, budget)
    }

    fn insert_loaded(
        &mut self,
        key: OwnedLoadedKey,
        source: LoadedSource,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<(), SourceStoreError> {
        let prepared = self.prepare_loaded(key, source, budget)?;
        self.value_mut().commit_loaded(prepared);
        Ok(())
    }
}

#[derive(Clone)]
struct StoreRoot {
    view: Arc<
        crate::compile_time::AdmittedShared<
            std::sync::Mutex<crate::compile_time::AdmittedOwned<StoreCache>>,
        >,
    >,
    limits: crate::compile_time::CompileTimeLimits,
}

struct StoreCall<'a, S: SourceStore> {
    resolver: &'a mut UnitResolver<S>,
    previous: Option<StoreRoot>,
}
impl<S: SourceStore> std::ops::Deref for StoreCall<'_, S> {
    type Target = UnitResolver<S>;
    fn deref(&self) -> &Self::Target {
        self.resolver
    }
}
impl<S: SourceStore> std::ops::DerefMut for StoreCall<'_, S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.resolver
    }
}
impl<S: SourceStore> Drop for StoreCall<'_, S> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.resolver.report.complete = false;
        }
        self.resolver.compile_time_store = self.previous.take();
    }
}

impl<S: SourceStore> UnitResolver<S> {
    fn callback_report_call(&mut self) -> CallbackReportCall<'_, S> {
        let previous = self.callback_reports.replace(CallbackReports::Requested);
        let previous_reason = self.callback_include_reason.take();
        let previous_unit_reason = self.callback_unit_reason.take();
        let previous_candidates = self.callback_unit_candidates.take();
        let previous_candidates_mode = std::mem::replace(&mut self.owning_unit_candidates, false);
        let previous_unit_reason_mode = std::mem::replace(&mut self.owning_unit_reason, false);
        let previous_include = std::mem::replace(&mut self.owning_include_callback, false);
        let previous_unit = std::mem::replace(&mut self.owning_unit_callback, false);
        let previous_names = self.callback_unit_names.take();
        let previous_provider = self.callback_provider.take();
        CallbackReportCall {
            resolver: self,
            previous,
            previous_reason,
            previous_unit_reason,
            previous_candidates,
            previous_candidates_mode,
            previous_unit_reason_mode,
            previous_include,
            previous_unit,
            previous_names,
            previous_provider,
        }
    }
    fn store_call<'b>(
        &'b mut self,
        budget: &mut crate::compile_time::CompileTimeBudget<'_>,
    ) -> Result<StoreCall<'b, S>, ResolverError> {
        let mut root = if let Some(root) = self
            .compile_time_caches
            .iter()
            .find(|r| r.view.is_admitted_by(budget))
        {
            root.clone()
        } else {
            let owner = budget
                .admit_owned(0, 0, std::mem::size_of::<StoreRoot>(), StoreCache::default)
                .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
            let view = budget
                .admit_owned(0, 0, 0, || std::sync::Mutex::new(owner))
                .and_then(|v| v.into_shared(budget))
                .and_then(|v| v.into_arc(budget))
                .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
            let root = StoreRoot {
                view,
                limits: budget.limits(),
            };
            let old = self.compile_time_caches.capacity();
            if self.compile_time_caches.len() == old {
                let new = (old + 1) * std::mem::size_of::<StoreRoot>();
                self.charge_compile_time(0, 0, new, budget.cancel_token())?;
                self.compile_time_caches.reserve_exact(1);
                self.release_compile_time(old * std::mem::size_of::<StoreRoot>());
            }
            self.compile_time_caches.push(root.clone());
            root
        };
        let other = self
            .compile_time_caches
            .iter()
            .filter(|r| !r.view.is_admitted_by(budget))
            .map(|r| r.view.budget_snapshot().retained_bytes)
            .sum::<usize>();
        let other = other.saturating_add(self.orphan_report_usage(Some(budget)).retained_bytes);
        let remaining = self
            .limits
            .max_compile_time_retained_bytes
            .saturating_sub(self.compile_time_accounting.retained_bytes)
            .saturating_sub(other);
        budget
            .constrain_retained(remaining)
            .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
        root.limits = budget.limits();
        let previous = self.compile_time_store.replace(root);
        Ok(StoreCall {
            resolver: self,
            previous,
        })
    }

    fn store_budget<'a>(
        &self,
        cancel: &'a dyn CancellationToken,
    ) -> Option<crate::compile_time::CompileTimeBudget<'a>> {
        self.compile_time_store
            .as_ref()
            .map(|root| root.view.store_budget(root.limits, cancel))
    }

    fn admit_lookup_storage(
        &mut self,
        work: usize,
        bytes: usize,
        retained: usize,
        cancel: &dyn CancellationToken,
    ) -> Result<(), ResolverError> {
        if let Some(root) = self.compile_time_store.clone() {
            let mut budget = root.view.store_budget(root.limits, cancel);
            let mut owner = root.view.lock().unwrap_or_else(|e| e.into_inner());
            owner
                .grow(&mut budget, work, bytes, retained, |_| ())
                .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
        }
        Ok(())
    }
    /// Cumulative admitted work and live scratch/published-output reservations.
    /// Temporary projections refund storage, never work or byte work. The
    /// resolver cannot observe callers dropping outputs, so their reservations
    /// conservatively last for this resolver's request-local lifetime.
    pub fn compile_time_usage(&self) -> crate::compile_time::BudgetSnapshot {
        let mut usage = self.compile_time_accounting;
        for root in &self.compile_time_caches {
            let live = root.view.budget_snapshot();
            let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
            usage.retained_bytes = usage.retained_bytes.saturating_add(live.retained_bytes);
            usage.work = usage
                .work
                .saturating_add(live.work.saturating_sub(cache.recorded_work));
            usage.byte_work = usage
                .byte_work
                .saturating_add(live.byte_work.saturating_sub(cache.recorded_byte_work));
        }
        let reports = self.orphan_report_usage(None);
        usage.retained_bytes = usage.retained_bytes.saturating_add(reports.retained_bytes);
        usage.work = usage.work.saturating_add(reports.work);
        usage.byte_work = usage.byte_work.saturating_add(reports.byte_work);
        usage
    }

    // Report-owned views keep earlier roots observable independently of cache
    // ownership. Count each existing ledger once, never a copied live total.
    fn orphan_report_usage(
        &self,
        exclude: Option<&CompileTimeBudget<'_>>,
    ) -> crate::compile_time::BudgetSnapshot {
        let mut usage = crate::compile_time::BudgetSnapshot::default();
        for (index, credit) in self.report_owners.credits().enumerate() {
            if exclude.is_some_and(|b| credit.is_admitted_by(b))
                || self
                    .compile_time_caches
                    .iter()
                    .any(|r| credit.shares_shared_root(&r.view))
                || self
                    .report_owners
                    .credits()
                    .take(index)
                    .any(|previous| credit.shares_root(previous))
            {
                continue;
            }
            let live = credit.budget_snapshot();
            usage.retained_bytes = usage.retained_bytes.saturating_add(live.retained_bytes);
            usage.work = usage
                .work
                .saturating_add(live.work.saturating_sub(credit.recorded_work));
            usage.byte_work = usage
                .byte_work
                .saturating_add(live.byte_work.saturating_sub(credit.recorded_byte_work));
        }
        usage
    }

    fn charge_compile_time(
        &mut self,
        work: usize,
        bytes: usize,
        retained: usize,
        cancel: &dyn CancellationToken,
    ) -> Result<(), ResolverError> {
        self.check_cancel(cancel)?;
        let admit = |current: usize, amount: usize, maximum: usize, limit| {
            let observed = current.checked_add(amount).unwrap_or(usize::MAX);
            if observed > maximum {
                Err(ResolverError::LimitExceeded {
                    limit,
                    observed,
                    maximum,
                })
            } else {
                Ok(observed)
            }
        };
        let c = self.compile_time_usage();
        let result: Result<_, ResolverError> = (|| {
            Ok(crate::compile_time::BudgetSnapshot {
                work: admit(
                    c.work,
                    work,
                    self.limits.max_compile_time_work,
                    "max_compile_time_work",
                )?,
                byte_work: admit(
                    c.byte_work,
                    bytes,
                    self.limits.max_compile_time_byte_work,
                    "max_compile_time_byte_work",
                )?,
                retained_bytes: admit(
                    c.retained_bytes,
                    retained,
                    self.limits.max_compile_time_retained_bytes,
                    "max_compile_time_retained_bytes",
                )?,
            })
        })();
        match result {
            Ok(_) => {
                self.compile_time_accounting.work += work;
                self.compile_time_accounting.byte_work += bytes;
                self.compile_time_accounting.retained_bytes += retained;
                Ok(())
            }
            Err(error) => {
                let _ = self.diagnostic_args(
                    cancel,
                    resolver_error_report_bound(&error),
                    format_args!("{error}"),
                );
                Err(error)
            }
        }
    }

    fn release_compile_time(&mut self, bytes: usize) {
        self.compile_time_accounting.retained_bytes = self
            .compile_time_accounting
            .retained_bytes
            .saturating_sub(bytes);
    }

    fn effective_root_route(&self, source: &LoadedSource) -> Option<LegacyRoute> {
        // A public ID is not a proof of path/revision or legacy authority.
        let inherited = if matches!(source.revision, SourceRevision::Overlay { .. }) {
            self.legacy_routes
                .get(&source.id)
                .filter(|route| path_equivalent(&route.source_path, &source.path))
        } else {
            None
        };
        self.legacy_route_for_source(source, inherited)
    }
    /// One typed textual-include session, borrowing this request-local resolver
    /// throughout. Source classification belongs to the root, not child paths.
    pub fn analyze_compile_time(
        &mut self,
        root: &LoadedSource,
        cancel: &dyn CancellationToken,
    ) -> Result<crate::compile_time::CompileTimeAnalysis, ResolverError> {
        self.check_cancel(cancel)?;
        // No conditional-decision cache: every root derives its actual context,
        // route and dependency observations. Keep same-root repeated includes
        // reusable, but revalidate payloads and negative directory observations
        // on the next root operation without resetting resource counters.
        self.loaded.retain(|id, _| {
            !self.include_seen.contains(id)
                && !self.compile_time_caches.iter().any(|root| {
                    root.view
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .include_seen
                        .contains(id)
                })
        });
        self.include_seen.clear();
        // Preserve the existing logical seen reset, not an opaque cache eviction.
        // Actual keys die; independently owned set backing stays charged/live.
        for root in &self.compile_time_caches {
            root.view
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .value_mut()
                .include_seen
                .clear();
        }
        self.directories.clear();
        let context = self.context.conditional_context_for(&root.path);
        let decode_reserve =
            if root.decoded_text.is_none() && std::str::from_utf8(&root.bytes).is_err() {
                loaded_source_retained(root).saturating_add(root.bytes.len().saturating_mul(3))
            } else {
                0
            };
        self.charge_compile_time(1, 0, decode_reserve, cancel)?;
        let decoded = if decode_reserve > 0 {
            let mut decoded = root.clone();
            decoded.decoded_text = Some(source_text(root).into());
            Some(decoded)
        } else {
            None
        };
        let root = decoded.as_ref().unwrap_or(root);
        let route = self.effective_root_route(root);
        let root_path_bytes = root.path.as_os_str().len() + std::mem::size_of::<PathBuf>();
        self.charge_compile_time(1, root_path_bytes, root_path_bytes, cancel)?;
        let previous_path =
            std::mem::replace(&mut self.compile_time_root_path, Some(root.path.clone()));
        let previous = std::mem::replace(&mut self.compile_time_route, route);
        let defaults = crate::compile_time::CompileTimeLimits::default();
        let aggregate = self.compile_time_usage();
        let limits = crate::compile_time::CompileTimeLimits {
            max_work: defaults.max_work.min(
                self.limits
                    .max_compile_time_work
                    .saturating_sub(aggregate.work),
            ),
            max_byte_work: defaults.max_byte_work.min(
                self.limits
                    .max_compile_time_byte_work
                    .saturating_sub(aggregate.byte_work),
            ),
            max_retained_bytes: defaults.max_retained_bytes.min(
                self.limits
                    .max_compile_time_retained_bytes
                    .saturating_sub(aggregate.retained_bytes),
            ),
            ..defaults
        };
        let mut budget = crate::compile_time::CompileTimeBudget::new(limits, cancel);
        let result = crate::compile_time::analyze_source_in_session(
            root,
            &context,
            self,
            &mut budget,
            cancel,
        );
        self.compile_time_route = previous;
        self.compile_time_root_path = previous_path;
        self.release_compile_time(root_path_bytes);
        if let Err(error) = &result {
            self.report.complete = false;
            // No active StoreCall remains here. Use the actual compilation
            // ledger explicitly, including failures before any store callback.
            // If cancelled/exhausted, retain the scalar diagnostic state and
            // prior evidence rather than allocate an unadmitted error string.
            if !budget.has_owned_ledger() {
                // The historical first metadata refusal precedes any typed
                // ledger/capability construction. Its caller-facing legacy
                // failure diagnostic keeps the existing plain-report contract;
                // no typed report credit exists to mint, transfer or refund.
                self.mark_incomplete(error.to_string());
            } else {
                let _ = self.report_message_in_budget(
                    true,
                    resolver_error_report_bound(error),
                    format_args!("{error}"),
                    &mut budget,
                );
            }
        }
        // Record already-performed work even when cancellation/error ends the
        // session. Failed-session storage has been dropped, never published.
        let usage = budget.snapshot();
        self.report_owners.record_work(&budget);
        self.compile_time_accounting.work += usage.work;
        self.compile_time_accounting.byte_work += usage.byte_work;
        if let Some(root) = self
            .compile_time_caches
            .iter()
            .find(|r| r.view.is_admitted_by(&budget))
        {
            let mut cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
            cache.value_mut().recorded_work = usage.work;
            cache.value_mut().recorded_byte_work = usage.byte_work;
        } else if result.is_ok() {
            self.compile_time_accounting.retained_bytes += usage.retained_bytes;
        }
        drop(decoded);
        self.release_compile_time(decode_reserve);
        result
    }

    fn source_order_analysis(
        &mut self,
        source: &LoadedSource,
        text: &str,
        retain_includes: bool,
        cancel: &dyn CancellationToken,
    ) -> Result<SourceOrderProjection, ResolverError> {
        let retained_before = self.compile_time_accounting.retained_bytes;
        let result = (|| {
            self.charge_compile_time(text.len().saturating_mul(8), text.len(), 0, cancel)?;
            let (has_includes, directives) =
                crate::conditional::source_directive_shape(text, cancel);
            if !has_includes {
                let context = self.context.conditional_context_for(&source.path);
                let mut context_bytes = 0usize;
                context
                    .visit_recovery_payload(&mut |n| {
                        context_bytes = context_bytes.saturating_add(n);
                        Ok(())
                    })
                    .expect("infallible size visitor");
                let entries =
                    context.defines.len() + context.options.len() + context.constants.len();
                let reserve =
                    text.len()
                        .saturating_mul(4)
                        .saturating_add((directives + 1).saturating_mul(
                            4 * (std::mem::size_of::<crate::conditional::ConditionalDirective>()
                                + 4 * std::mem::size_of::<Range<usize>>()
                                + context_bytes
                                + entries * 64),
                        ));
                self.charge_compile_time(
                    entries.saturating_mul(directives + 1),
                    context_bytes,
                    reserve,
                    cancel,
                )?;
                let mut lexical =
                    crate::conditional::analyze_with_context_and_cancel(text, &context, cancel);
                let unsupported = retain_includes
                    && lexical.directives.iter().any(|d| {
                        d.potentially_active() && d.kind == crate::conditional::DirectiveKind::Other
                    });
                if unsupported {
                    lexical.complete = false;
                    self.mark_incomplete(format!(
                        "source {} contains an unsupported active directive",
                        source.path.display()
                    ));
                }
                let bytes = conditional_analysis_retained(&lexical);
                if bytes > reserve {
                    self.charge_compile_time(1, 0, bytes - reserve, cancel)?;
                } else {
                    self.release_compile_time(reserve - bytes);
                }
                let includes = retain_includes.then(|| IncludeWalkResult {
                    includes: vec![],
                    complete: lexical.complete && lexical.unknown_spans.is_empty(),
                });
                return Ok(SourceOrderProjection {
                    root: lexical,
                    retained: bytes,
                    includes,
                });
            }
            let scratch = loaded_source_retained(source).saturating_add(text.len());
            self.charge_compile_time(1, text.len(), scratch, cancel)?;
            let mut root = source.clone();
            root.decoded_text = Some(text.into());
            let result = self.analyze_compile_time(&root, cancel);
            drop(root);
            self.release_compile_time(scratch);
            let mut analysis = result?;
            let unsupported = analysis.occurrences.iter().any(|o| {
                o.analysis.directives.iter().any(|d| {
                    d.potentially_active() && d.kind == crate::conditional::DirectiveKind::Other
                })
            });
            if unsupported {
                analysis.value_mut().root.complete = false;
                self.mark_incomplete(format!(
                    "source {} contains an unsupported active directive",
                    source.path.display()
                ));
            }
            let includes = if retain_includes {
                let count = analysis
                    .directive_fragments
                    .iter()
                    .filter(|m| m.directive.kind == crate::conditional::DirectiveKind::Include)
                    .count();
                self.charge_compile_time(
                    analysis.directive_fragments.len(),
                    0,
                    count * std::mem::size_of::<ResolvedInclude>(),
                    cancel,
                )?;
                let mut includes = Vec::with_capacity(count);
                for mapping in analysis
                    .directive_fragments
                    .iter()
                    .filter(|m| m.directive.kind == crate::conditional::DirectiveKind::Include)
                {
                    let directive = &mapping.directive;
                    let anchor = mapping
                        .fragments
                        .first()
                        .expect("directive physical opener");
                    self.charge_compile_time(
                        analysis.includes.len() + 1,
                        directive.body.len(),
                        directive.body.len(),
                        cancel,
                    )?;
                    let Some(name) = include_name(&directive.body) else {
                        self.release_compile_time(directive.body.len());
                        continue;
                    };
                    self.release_compile_time(directive.body.len() - name.len());
                    let target = analysis.includes.iter().find(|i| i.directive == *anchor);
                    let target_bytes = match target.map(|i| &i.target) {
                        Some(Resolution::Found(source)) if !unsupported => source.id.as_str().len(),
                        _ => 0,
                    };
                    self.charge_compile_time(
                        1,
                        0,
                        anchor.source.as_str().len() + target_bytes,
                        cancel,
                    )?;
                    let target = match target.map(|i| &i.target) {
                        Some(Resolution::Found(source)) if !unsupported => {
                            ResolutionTarget::Found(source.id.clone())
                        }
                        Some(Resolution::Ambiguous { .. }) => ResolutionTarget::Ambiguous,
                        Some(Resolution::Unavailable { .. })
                            if directive.activity == crate::conditional::Truth::False =>
                        {
                            ResolutionTarget::Unavailable
                        }
                        None if directive.activity == crate::conditional::Truth::False => {
                            ResolutionTarget::Unavailable
                        }
                        _ => ResolutionTarget::Incomplete,
                    };
                    includes.push(ResolvedInclude {
                        including_source_id: anchor.source.clone(),
                        byte_range: anchor.range.clone(),
                        requested_name: name,
                        target,
                    });
                }
                Some(IncludeWalkResult {
                    includes,
                    complete: analysis.root.complete && analysis.root.unknown_spans.is_empty(),
                })
            } else {
                None
            };
            if !analysis.root.complete {
                self.mark_incomplete(format!(
                    "conditional analysis is incomplete for {}",
                    source.path.display()
                ));
            }
            let bytes = conditional_analysis_retained(&analysis.root);
            // Transfer the existing immutable projection; no root clone or cache
            // insertion. Drop the graphs/occurrences and refund their reservation.
            let typed_usage = analysis.budget.budget_snapshot().retained_bytes;
            if bytes > typed_usage {
                self.charge_compile_time(1, 0, bytes - typed_usage, cancel)?;
            } else {
                self.release_compile_time(typed_usage - bytes);
            }
            Ok(SourceOrderProjection {
                root: analysis.root.clone(),
                retained: bytes,
                includes,
            })
        })();
        if result.is_err() {
            self.compile_time_accounting.retained_bytes = retained_before;
        }
        result
    }

    pub fn new(
        context: ProjectContext,
        workspace_roots: Vec<PathBuf>,
        store: S,
        limits: ResolverLimits,
    ) -> Self {
        Self::new_inner(context, workspace_roots, store, limits, None)
    }

    /// Create a resolver that can reuse package catalogues from other
    /// resolvers sharing the same immutable project context, resolver limits,
    /// and unchanged source-store state in one operation. Discard the cache
    /// when that operation completes.
    pub fn new_with_session_cache(
        context: ProjectContext,
        workspace_roots: Vec<PathBuf>,
        store: S,
        limits: ResolverLimits,
        session_cache: ResolverSessionCache,
    ) -> Self {
        Self::new_inner(context, workspace_roots, store, limits, Some(session_cache))
    }

    fn new_inner(
        context: ProjectContext,
        workspace_roots: Vec<PathBuf>,
        store: S,
        limits: ResolverLimits,
        session_cache: Option<ResolverSessionCache>,
    ) -> Self {
        let session_cache = session_cache.filter(|cache| cache.bind(&context, limits));
        let mut report = ResolutionReport {
            observations: Vec::new(),
            warnings: Vec::new(),
            complete: context.discovery_complete,
            incomplete_reasons: Vec::new(),
        };
        for observation in &context.metadata_observations {
            push_observation(
                &mut report.observations,
                ResolutionObservation::Metadata(observation.clone()),
            );
        }
        for warning in &context.warnings {
            push_warning(&mut report, warning.clone(), limits.max_resolution_warnings);
        }
        if !context.discovery_complete {
            report
                .incomplete_reasons
                .push("project metadata is incomplete".to_string());
        }
        Self {
            context,
            workspace_roots: workspace_roots
                .into_iter()
                .map(|root| canonical_path(&root))
                .collect(),
            store,
            limits,
            report,
            report_owners: ReportOwners::default(),
            loaded: HashMap::new(),
            directories: HashMap::new(),
            package_catalogues: HashMap::new(),
            session_cache,
            unit_cache: HashMap::new(),
            legacy_routes: HashMap::new(),
            project_unit_ids: None,
            include_files: 0,
            include_bytes: 0,
            include_directives: 0,
            package_lookups: 0,
            package_catalogue_count: 0,
            scanned_entries: 0,
            scanned_bytes: 0,
            project_includes: Vec::new(),
            include_seen: HashSet::new(),
            source_metadata: HashMap::new(),
            stop_when_incomplete: false,
            compile_time_route: None,
            compile_time_root_path: None,
            compile_time_accounting: Default::default(),
            compile_time_store: None,
            compile_time_caches: Vec::new(),
            callback_reports: None,
            callback_include_reason: None,
            callback_unit_reason: None,
            callback_unit_candidates: None,
            owning_unit_candidates: false,
            owning_unit_reason: false,
            owning_include_callback: false,
            owning_unit_callback: false,
            callback_unit_names: None,
            callback_provider: None,
        }
    }

    /// Declared name and imports of a loaded source, parsed at most once per
    /// resolver for the same content. Each importer reloads its candidates, so
    /// the cache compares content rather than allocation identity.
    fn source_metadata(&mut self, source: &LoadedSource) -> Arc<ParsedSourceMetadata> {
        if let Some(cached) = self
            .source_metadata
            .get(&source.id)
            .filter(|cached| cached.matches(source))
        {
            return cached.metadata.clone();
        }
        let metadata = Arc::new(parse_source_metadata(source));
        self.source_metadata.insert(
            source.id.clone(),
            CachedSourceMetadata {
                bytes: source.bytes.clone(),
                decoded_text: source.decoded_text.clone(),
                metadata: metadata.clone(),
            },
        );
        metadata
    }

    /// Stop project walks as soon as the graph is known to be incomplete.
    ///
    /// Callers that discard incomplete graphs use this to avoid resolving the
    /// rest of the dependency closure. The report still covers every source
    /// read before stopping, which is what proved the graph incomplete.
    pub fn stop_project_walk_when_incomplete(&mut self, stop: bool) {
        self.stop_when_incomplete = stop;
    }

    pub fn resolve_unit(
        &mut self,
        request: UnitResolveRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> ResolutionOutcome<ResolvedUnit> {
        let before_observations = self.report.observations.len();
        let before_warnings = self.report.warnings.len();
        let result = self.resolve_unit_result(request, cancel);
        self.outcome(result, before_observations, before_warnings, cancel)
            .unwrap_or_else(|error| ResolutionOutcome {
                result: Resolution::Incomplete {
                    reason: error.to_string(),
                    candidates: vec![],
                },
                observations: vec![],
                warnings: vec![],
            })
    }

    /// Result-preserving variant for callers that must distinguish
    /// cancellation from an ordinary incomplete lookup.
    pub fn try_resolve_unit(
        &mut self,
        request: UnitResolveRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<ResolutionOutcome<ResolvedUnit>, ResolverError> {
        let before_observations = self.report.observations.len();
        let before_warnings = self.report.warnings.len();
        let result = self.resolve_unit_result(request, cancel).map_err(|error| {
            self.report.complete = false;
            let _ = self.diagnostic_args(
                cancel,
                resolver_error_report_bound(&error),
                format_args!("{error}"),
            );
            error
        })?;
        self.outcome(Ok(result), before_observations, before_warnings, cancel)
    }

    /// Load one caller-selected source through the same authorization and
    /// observation path used by unit candidates.
    ///
    /// LSP and other stateful callers may already have selected the document
    /// that owns an import list. They must not resolve that document by name:
    /// doing so could turn an accepted root into an ambiguity merely because a
    /// second same-named file is present. The caller still supplies the exact
    /// legacy route proof, when applicable.
    pub fn load_source(
        &mut self,
        path: &Path,
        legacy_route: Option<&LegacyRoute>,
        kind: SourceKind,
        cancel: &dyn CancellationToken,
    ) -> Result<LoadedSource, ResolverError> {
        self.check_cancel(cancel)?;
        let path = canonical_path(path);
        // The project's entries carry on-disk spellings; a caller's 8.3 short
        // or other-case spelling of the same file must match them.
        let path = path_identity::on_disk_spelling(&path).unwrap_or(path);
        let entry = self
            .context
            .path_entry_for(&path)
            .or_else(|| self.legacy_entry_for_path(&path, legacy_route))
            .ok_or_else(|| {
                ResolverError::InvalidRequest(format!(
                    "source {} is outside the requester-scoped project roots",
                    path.display()
                ))
            })?;
        let source = self
            .load_candidate_with_limit(
                &path,
                &entry,
                legacy_route,
                kind,
                self.limits.max_source_bytes,
                cancel,
            )
            .map_err(|error| match error {
                SourceStoreError::Cancelled => ResolverError::Cancelled,
                error => ResolverError::SourceStore(error),
            })?;
        if let Some(route) = self.legacy_route_for_source(&source, legacy_route) {
            self.legacy_routes.insert(source.id.clone(), route);
        }
        Ok(source)
    }

    pub fn resolve_imports(
        &mut self,
        importer: &ResolvedUnit,
        sites: &[ImportSite],
        cancel: &dyn CancellationToken,
    ) -> Result<ResolvedImports, ResolverError> {
        let (text, require_byte_preserving_coordinates) =
            importer.source.decoded_text.as_deref().map_or_else(
                || {
                    (
                        crate::text::decode_bytes(&importer.source.bytes).into_owned(),
                        true,
                    )
                },
                |text| (text.to_owned(), false),
            );
        self.resolve_imports_inner(
            importer,
            sites,
            &text,
            require_byte_preserving_coordinates,
            None,
            false,
            cancel,
        )
        .map(|(resolved, _)| resolved)
    }

    /// Resolve imports whose ranges were produced from a caller-owned decoded
    /// source string.  LSP indexes use UTF-8 byte offsets in decoded text even
    /// when the on-disk payload is UTF-16, so those ranges must not be compared
    /// with the raw payload length.  The raw source revision remains attached
    /// to every loaded dependency for stale-result validation.
    pub fn resolve_imports_with_text(
        &mut self,
        importer: &ResolvedUnit,
        sites: &[ImportSite],
        text: &str,
        cancel: &dyn CancellationToken,
    ) -> Result<ResolvedImports, ResolverError> {
        self.resolve_imports_inner(importer, sites, text, false, None, false, cancel)
            .map(|(resolved, _)| resolved)
    }

    /// Resolve imports while withholding browsing-only source fallback for
    /// names that a caller has independently validated as compiled providers.
    /// Explicit bindings, normal search paths, and conditional activity retain
    /// their usual precedence; this does not itself select a compiled result.
    pub fn resolve_imports_with_text_skipping_browsing(
        &mut self,
        importer: &ResolvedUnit,
        sites: &[ImportSite],
        text: &str,
        validated_compiled_names: &[String],
        cancel: &dyn CancellationToken,
    ) -> Result<ResolvedImports, ResolverError> {
        let names = validated_compiled_names
            .iter()
            .map(|name| name.to_ascii_lowercase())
            .collect::<HashSet<_>>();
        self.resolve_imports_inner(importer, sites, text, false, Some(&names), false, cancel)
            .map(|(resolved, _)| resolved)
    }

    fn resolve_imports_inner(
        &mut self,
        importer: &ResolvedUnit,
        sites: &[ImportSite],
        text: &str,
        require_byte_preserving_coordinates: bool,
        validated_compiled_names: Option<&HashSet<String>>,
        retain_includes: bool,
        cancel: &dyn CancellationToken,
    ) -> Result<(ResolvedImports, Option<IncludeWalkResult>), ResolverError> {
        let before = self.compile_time_accounting.retained_bytes;
        let projection =
            self.source_order_analysis(&importer.source, text, retain_includes, cancel)?;
        let result = self.resolve_imports_admitted(
            importer,
            sites,
            text,
            require_byte_preserving_coordinates,
            validated_compiled_names,
            &projection.root,
            cancel,
        );
        self.release_compile_time(projection.retained);
        if result.is_err() {
            self.compile_time_accounting.retained_bytes = before;
        }
        result.map(|resolved| (resolved, projection.includes))
    }

    fn resolve_imports_admitted(
        &mut self,
        importer: &ResolvedUnit,
        sites: &[ImportSite],
        text: &str,
        require_byte_preserving_coordinates: bool,
        validated_compiled_names: Option<&HashSet<String>>,
        analysis: &crate::conditional::ConditionalAnalysis,
        cancel: &dyn CancellationToken,
    ) -> Result<ResolvedImports, ResolverError> {
        let payload = std::mem::size_of::<ResolvedImports>()
            + sites.len() * std::mem::size_of::<ResolvedImport>()
            + sites
                .iter()
                .map(|s| s.requested_name.len() + importer.source.id.as_str().len())
                .sum::<usize>();
        self.charge_compile_time(sites.len() + 1, 0, payload, cancel)?;
        let mut bindings = Vec::with_capacity(sites.len());
        let mut dependencies = Vec::new();
        let mut dependency_ids = HashSet::new();
        let mut complete = true;
        self.check_cancel(cancel)?;
        if (require_byte_preserving_coordinates && text.len() != importer.source.bytes.len())
            || !analysis.complete
        {
            complete = false;
            self.mark_incomplete(format!(
                "conditional analysis is incomplete for {}",
                importer.source.path.display()
            ));
        } else if !analysis.unknown_spans.is_empty() {
            complete = false;
            self.mark_incomplete(format!(
                "source {} contains unknown conditional activity",
                importer.source.path.display()
            ));
        }
        for site in sites {
            self.check_cancel(cancel)?;
            let importer_route = self.effective_root_route(&importer.source);
            let key = UnitCacheKey::new(
                &importer.source.path,
                &site.requested_name,
                importer_route.as_ref(),
            );
            let inactive = analysis_contains_range(&analysis.inactive_spans, &site.byte_range);
            let unknown = (require_byte_preserving_coordinates
                && text.len() != importer.source.bytes.len())
                || !analysis.complete
                || analysis_contains_range(&analysis.unknown_spans, &site.byte_range);
            let outcome =
                if inactive {
                    Resolution::Unavailable {
                        reason: "import is in a known inactive conditional branch".to_string(),
                    }
                } else if unknown {
                    Resolution::Incomplete {
                        reason: "import conditional activity is unknown".to_string(),
                        candidates: Vec::new(),
                    }
                } else if let Some(unit) = self.unit_cache.get(&key).cloned() {
                    Resolution::Found(unit)
                } else if self.project_unit_ids.as_ref().is_some_and(|ids| {
                    ids.len().saturating_sub(1) >= self.limits.max_dependency_units
                }) {
                    complete = false;
                    self.mark_incomplete(format!(
                        "dependency unit limit ({}) reached",
                        self.limits.max_dependency_units
                    ));
                    Resolution::Incomplete {
                        reason: format!(
                            "dependency unit limit ({}) reached",
                            self.limits.max_dependency_units
                        ),
                        candidates: Vec::new(),
                    }
                } else {
                    let aliased = aliased_name(&self.context, &site.requested_name);
                    let suppress_browsing = validated_compiled_names.is_some_and(|names| {
                        names.contains(&site.requested_name.to_ascii_lowercase())
                            || names.contains(&aliased.to_ascii_lowercase())
                    });
                    self.resolve_unit_result_with_browsing(
                        UnitResolveRequest {
                            requested_name: &site.requested_name,
                            importer_path: &importer.source.path,
                            legacy_route: importer_route.as_ref(),
                        },
                        suppress_browsing,
                        cancel,
                    )?
                };
            let lookup_bytes = aliased_name_ref(&self.context, &site.requested_name).len();
            let declared_bytes = match &outcome {
                Resolution::Found(unit) => unit.declared_name.len(),
                _ => 0,
            };
            self.charge_compile_time(
                1,
                0,
                8 * std::mem::size_of::<String>()
                    + 4 * (site.requested_name.len() + lookup_bytes + declared_bytes),
                cancel,
            )?;
            let (target, authorized_qualifiers) = match outcome {
                Resolution::Found(unit) => {
                    self.charge_compile_time(
                        1,
                        0,
                        unit.source.id.as_str().len().saturating_mul(4),
                        cancel,
                    )?;
                    let target = unit.source.id.clone();
                    let dependency_allowed = self.project_unit_ids.as_ref().is_none_or(|ids| {
                        ids.contains(&target)
                            || ids.len().saturating_sub(1) < self.limits.max_dependency_units
                    });
                    if !dependency_allowed {
                        complete = false;
                        self.mark_incomplete(format!(
                            "dependency unit limit ({}) reached",
                            self.limits.max_dependency_units
                        ));
                        (
                            ResolutionTarget::Incomplete,
                            qualifiers_for_unresolved(
                                &site.requested_name,
                                &aliased_name(&self.context, &site.requested_name),
                            ),
                        )
                    } else {
                        let new_dependency = !dependency_ids.contains(&target)
                            && self
                                .project_unit_ids
                                .as_ref()
                                .is_none_or(|ids| !ids.contains(&target));
                        if new_dependency {
                            let bytes = loaded_source_retained(&unit.source)
                                + unit.requested_name.len()
                                + unit.declared_name.len()
                                + 2 * std::mem::size_of::<ResolvedUnit>()
                                + 8 * std::mem::size_of::<SourceId>();
                            self.charge_compile_time(1, 0, bytes, cancel)?;
                            dependencies.reserve_exact(1);
                        }
                        if self.project_unit_ids.is_none() && dependency_ids.insert(target.clone())
                        {
                            dependencies.push(unit.clone());
                        } else if self
                            .project_unit_ids
                            .as_mut()
                            .is_some_and(|ids| ids.insert(target.clone()))
                        {
                            dependency_ids.insert(target.clone());
                            dependencies.push(unit.clone());
                        }
                        (
                            ResolutionTarget::Found(target),
                            qualifiers_for_found(
                                &site.requested_name,
                                &aliased_name(&self.context, &site.requested_name),
                                &unit.declared_name,
                            ),
                        )
                    }
                }
                Resolution::Unavailable { .. } => {
                    if !inactive {
                        complete = false;
                    }
                    (
                        ResolutionTarget::Unavailable,
                        qualifiers_for_unresolved(
                            &site.requested_name,
                            &aliased_name(&self.context, &site.requested_name),
                        ),
                    )
                }
                Resolution::Ambiguous { .. } => {
                    complete = false;
                    (
                        ResolutionTarget::Ambiguous,
                        qualifiers_for_unresolved(
                            &site.requested_name,
                            &aliased_name(&self.context, &site.requested_name),
                        ),
                    )
                }
                Resolution::Incomplete { .. } => {
                    complete = false;
                    (
                        ResolutionTarget::Incomplete,
                        qualifiers_for_unresolved(
                            &site.requested_name,
                            &aliased_name(&self.context, &site.requested_name),
                        ),
                    )
                }
            };
            bindings.push(ResolvedImport {
                importer_source_id: importer.source.id.clone(),
                site: site.clone(),
                target,
                authorized_qualifiers,
            });
        }
        self.check_cancel(cancel)?;
        if !complete {
            self.mark_incomplete("import resolution was incomplete".to_string());
        }
        Ok(ResolvedImports {
            bindings,
            dependencies,
            complete,
        })
    }

    pub fn resolve_include(
        &mut self,
        request: IncludeResolveRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> ResolutionOutcome<ResolvedSource> {
        let before_observations = self.report.observations.len();
        let before_warnings = self.report.warnings.len();
        let result = self.resolve_include_result(request, cancel);
        self.outcome(result, before_observations, before_warnings, cancel)
            .unwrap_or_else(|error| ResolutionOutcome {
                result: Resolution::Incomplete {
                    reason: error.to_string(),
                    candidates: vec![],
                },
                observations: vec![],
                warnings: vec![],
            })
    }

    /// Result-preserving variant for callers that must distinguish
    /// cancellation from an ordinary incomplete include lookup.
    pub fn try_resolve_include(
        &mut self,
        request: IncludeResolveRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<ResolutionOutcome<ResolvedSource>, ResolverError> {
        let before_observations = self.report.observations.len();
        let before_warnings = self.report.warnings.len();
        let result = self
            .resolve_include_result(request, cancel)
            .map_err(|error| {
                self.report.complete = false;
                let _ = self.diagnostic_args(
                    cancel,
                    resolver_error_report_bound(&error),
                    format_args!("{error}"),
                );
                error
            })?;
        self.outcome(Ok(result), before_observations, before_warnings, cancel)
    }

    pub fn resolve_project(
        &mut self,
        root: UnitResolveRequest<'_>,
        sites: &[ImportSite],
        cancel: &dyn CancellationToken,
    ) -> Result<ResolvedProject, ResolverError> {
        let root = match self.resolve_unit_result(root, cancel)? {
            Resolution::Found(root) => root,
            Resolution::Unavailable { reason } | Resolution::Incomplete { reason, .. } => {
                return Err(ResolverError::InvalidRequest(format!(
                    "root unit unavailable: {reason}"
                )));
            }
            Resolution::Ambiguous { .. } => {
                return Err(ResolverError::InvalidRequest(
                    "root unit is ambiguous".to_string(),
                ));
            }
        };
        self.resolve_project_with_root(root, sites, cancel)
    }

    /// Walk a project from a caller-selected, authorized source path.
    ///
    /// The selected root is loaded through [`Self::load_source`] and is never
    /// sent through unit alias lookup or filename precedence.  Aliases and
    /// normal candidate precedence remain active for imports encountered while
    /// walking the project.  This distinction is required when a project alias
    /// happens to match the selected root's declared name.
    pub fn resolve_project_from_source(
        &mut self,
        path: &Path,
        requested_name: &str,
        sites: &[ImportSite],
        legacy_route: Option<&LegacyRoute>,
        cancel: &dyn CancellationToken,
    ) -> Result<ResolvedProject, ResolverError> {
        self.check_cancel(cancel)?;
        let source = self.load_source(path, legacy_route, SourceKind::Unit, cancel)?;
        let metadata = self.source_metadata(&source);
        let declared_name = metadata
            .declared_name
            .clone()
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| {
                ResolverError::InvalidRequest(format!(
                    "selected root {} has no unit/program declaration",
                    source.path.display()
                ))
            })?;
        let requested_name = requested_name.trim();
        if requested_name.is_empty() {
            return Err(ResolverError::InvalidRequest(
                "selected root name is empty".to_string(),
            ));
        }
        self.resolve_project_with_root(
            ResolvedUnit {
                requested_name: requested_name.to_string(),
                declared_name,
                source,
            },
            sites,
            cancel,
        )
    }

    fn resolve_project_with_root(
        &mut self,
        root: ResolvedUnit,
        sites: &[ImportSite],
        cancel: &dyn CancellationToken,
    ) -> Result<ResolvedProject, ResolverError> {
        let previous_project_unit_ids = self.project_unit_ids.take();
        let before = self.compile_time_accounting.retained_bytes;
        let result = self.resolve_project_inner(root, sites, cancel);
        self.project_unit_ids = previous_project_unit_ids;
        if result.is_err() {
            self.project_includes = Vec::new();
            self.compile_time_accounting.retained_bytes = before;
        }
        result
    }

    fn resolve_project_inner(
        &mut self,
        root: ResolvedUnit,
        sites: &[ImportSite],
        cancel: &dyn CancellationToken,
    ) -> Result<ResolvedProject, ResolverError> {
        let mut queue = ProjectImportQueue::default();
        let result = self.resolve_project_queued(root, sites, cancel, &mut queue);
        let retained = queue.site_retained + queue.storage_retained;
        // This covers success, early abandonment, errors and cancellation.
        // Drop the actual buffers before refunding them; work is never refunded.
        drop(queue);
        self.release_compile_time(retained);
        result
    }

    fn enqueue_project_imports(
        &mut self,
        queue: &mut ProjectImportQueue,
        unit: &ResolvedUnit,
        sites: &[ImportSite],
        cancel: &dyn CancellationToken,
    ) -> Result<(), ResolverError> {
        let mut retained = sites
            .len()
            .saturating_mul(std::mem::size_of::<ImportSite>());
        for site in sites {
            self.charge_compile_time(
                1,
                std::mem::size_of::<ImportSite>().saturating_add(site.requested_name.len()),
                0,
                cancel,
            )?;
            retained = retained.saturating_add(site.requested_name.len());
        }
        let additional = (queue.items.len() + 1).saturating_sub(queue.items.capacity());
        // A growth may temporarily coexist with the old backing allocation.
        let storage = if additional > 0 {
            (queue.items.len() + 1).saturating_mul(std::mem::size_of::<QueuedProjectImporter>())
        } else {
            0
        };
        let moved_entries = if additional > 0 { queue.items.len() } else { 0 };
        self.charge_compile_time(
            moved_entries + 1,
            moved_entries.saturating_mul(std::mem::size_of::<QueuedProjectImporter>()),
            retained.saturating_add(storage),
            cancel,
        )?;
        // reserve_exact avoids unadmitted geometric queue capacity growth.
        if additional > 0 {
            queue.items.reserve_exact(1);
            self.release_compile_time(queue.storage_retained);
            queue.storage_retained = storage;
        }
        queue.items.push_back(QueuedProjectImporter {
            unit: unit.clone(),
            sites: sites.to_vec(),
            site_retained: retained,
        });
        queue.site_retained += retained;
        Ok(())
    }

    fn resolve_project_queued(
        &mut self,
        root: ResolvedUnit,
        sites: &[ImportSite],
        cancel: &dyn CancellationToken,
        queue: &mut ProjectImportQueue,
    ) -> Result<ResolvedProject, ResolverError> {
        self.check_cancel(cancel)?;
        self.charge_compile_time(
            1,
            0,
            loaded_source_retained(&root.source)
                + root.requested_name.len()
                + root.declared_name.len()
                + std::mem::size_of::<ResolvedUnit>(),
            cancel,
        )?;
        let metadata;
        let root_sites = if sites.is_empty() {
            metadata = self.source_metadata(&root.source);
            &metadata.imports
        } else {
            sites
        };
        let mut imports = Vec::new();
        let mut units = Vec::new();
        let mut include_sources = Vec::new();
        let mut loaded_ids = HashSet::from([root.source.id.clone()]);
        self.project_unit_ids = Some(loaded_ids.clone());
        self.enqueue_project_imports(queue, &root, root_sites, cancel)?;
        let mut complete = self.context.discovery_complete && self.report.complete;
        while let Some(item) = queue.items.pop_front() {
            let step = (|| {
                let importer = &item.unit;
                let importer_sites = &item.sites;
                self.check_cancel(cancel)?;
                if self.stop_when_incomplete && !complete {
                    return Ok(false);
                }
                let text = source_text(&importer.source);
                let (resolved, include_result) = self.resolve_imports_inner(
                    &importer,
                    &importer_sites,
                    &text,
                    importer.source.decoded_text.is_none(),
                    None,
                    true,
                    cancel,
                )?;
                complete &= resolved.complete;
                let binding_storage =
                    resolved.bindings.capacity() * std::mem::size_of::<ResolvedImport>();
                let additional =
                    (imports.len() + resolved.bindings.len()).saturating_sub(imports.capacity());
                self.charge_compile_time(
                    resolved.bindings.len(),
                    0,
                    additional * std::mem::size_of::<ResolvedImport>(),
                    cancel,
                )?;
                if additional > 0 {
                    imports.reserve_exact(resolved.bindings.len());
                }
                imports.extend(resolved.bindings);
                self.release_compile_time(binding_storage + std::mem::size_of::<ResolvedImports>());
                for dependency in resolved.dependencies {
                    if loaded_ids.insert(dependency.source.id.clone()) {
                        let metadata = self.source_metadata(&dependency.source);
                        self.enqueue_project_imports(
                            queue,
                            &dependency,
                            &metadata.imports,
                            cancel,
                        )?;
                        units.push(dependency);
                    }
                }
                if let Some(include_result) = include_result {
                    self.append_include_result(
                        include_result,
                        &mut complete,
                        &mut include_sources,
                        cancel,
                    )?;
                }
                Ok::<_, ResolverError>(true)
            })();
            let retained = item.site_retained;
            drop(item);
            queue.site_retained -= retained;
            self.release_compile_time(retained);
            if !step? {
                break;
            }
        }

        let includes = self.take_project_includes();
        include_sources.retain(|source| !loaded_ids.contains(&source.id));
        let mut report = self.finish_report();
        report.complete &= complete;
        if !complete && report.incomplete_reasons.is_empty() {
            report
                .incomplete_reasons
                .push("project resolution was incomplete".to_string());
        }
        Ok(ResolvedProject {
            conditional_contexts: {
                let mut contexts = HashMap::new();
                for unit in std::iter::once(&root).chain(units.iter()) {
                    self.charge_compile_time(
                        1,
                        0,
                        std::mem::size_of::<(SourceId, pascal_project::ConditionalContext)>()
                            + unit.source.id.as_str().len(),
                        cancel,
                    )?;
                    // Admit all possible selected-context payload before the
                    // ProjectContext compatibility projection clones it.
                    let reserve = conditional_context_reserve(&self.context)?;
                    self.charge_compile_time(1, reserve, reserve, cancel)?;
                    contexts.insert(
                        unit.source.id.clone(),
                        self.context.conditional_context_for(&unit.source.path),
                    );
                }
                contexts
            },
            root,
            units,
            imports,
            includes,
            include_sources,
            complete,
            report,
        })
    }

    pub fn finish(self) -> ResolutionReport {
        self.report_owners.into_raw_floor();
        self.report
    }

    /// Consume the persistent report without cloning. The admitted envelope
    /// owns all original per-field/per-root receipts; other resolver storage
    /// drops independently. A refusal destroys report storage normally.
    pub fn finish_admitted(
        self,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<AdmittedOwned<AdmittedResolutionReport>, ResolverError> {
        let other = self
            .compile_time_caches
            .iter()
            .filter(|root| !root.view.is_admitted_by(budget))
            .map(|root| root.view.budget_snapshot().retained_bytes)
            .fold(
                self.compile_time_accounting.retained_bytes,
                usize::saturating_add,
            )
            .saturating_add(self.orphan_report_usage(Some(budget)).retained_bytes);
        budget
            .constrain_retained(
                self.limits
                    .max_compile_time_retained_bytes
                    .saturating_sub(other),
            )
            .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
        budget
            .admit_owned(0, 0, 0, || AdmittedResolutionReport {
                report: self.report,
                owners: self.report_owners,
            })
            .map_err(|r| crate::compile_time::resolver_error(r, budget))
    }

    /// Adjust the request-wide include budget before another include lookup.
    /// The limits are absolute counters, so callers that combine this session
    /// with an outer auditor can account for work performed by other contexts.
    pub fn set_include_limits(
        &mut self,
        max_files: usize,
        max_bytes: usize,
        max_directives: usize,
    ) {
        self.limits.max_include_files = max_files;
        self.limits.max_include_bytes = max_bytes;
        self.limits.max_include_directives = max_directives;
    }

    pub fn include_usage(&self) -> (usize, usize, usize) {
        (
            self.include_files,
            self.include_bytes,
            self.include_directives,
        )
    }

    pub fn report(&self) -> ResolutionReport {
        self.report.clone()
    }

    fn outcome<T>(
        &mut self,
        result: Result<Resolution<T>, ResolverError>,
        before_observations: usize,
        before_warnings: usize,
        cancel: &dyn CancellationToken,
    ) -> Result<ResolutionOutcome<T>, ResolverError> {
        let result = match result {
            Ok(result) => {
                if matches!(&result, Resolution::Incomplete { .. }) {
                    self.diagnostic_args(
                        cancel,
                        37,
                        format_args!("resolution outcome was incomplete"),
                    )?;
                }
                result
            }
            Err(error) => {
                self.diagnostic_args(
                    cancel,
                    resolver_error_report_bound(&error),
                    format_args!("{error}"),
                )?;
                let reason = if self.owning_unit_reason {
                    self.unit_reason_build(
                        resolver_error_report_bound(&error),
                        cancel,
                        format_args!("{error}"),
                    )?
                } else if self.owning_include_callback {
                    self.include_reason_build(
                        resolver_error_report_bound(&error),
                        cancel,
                        format_args!("{error}"),
                    )?
                } else {
                    error.to_string()
                };
                Resolution::Incomplete {
                    reason,
                    candidates: Vec::new(),
                }
            }
        };
        if self.callback_reports.is_some() {
            let mut budget = self.store_budget(cancel).ok_or_else(|| {
                ResolverError::InvalidRequest(
                    "returned report requires an active admitted store call".into(),
                )
            })?;
            let reports = AdmittedCapabilityReports::copy(
                &self.report.observations[before_observations..],
                &self.report.warnings[before_warnings..],
                &mut budget,
            )
            .map_err(|error| {
                self.report.complete = false;
                error
            })?;
            self.callback_reports = Some(CallbackReports::Built(reports));
            return Ok(ResolutionOutcome {
                result,
                observations: Vec::new(),
                warnings: Vec::new(),
            });
        }
        Ok(ResolutionOutcome {
            result,
            observations: self.report.observations[before_observations..].to_vec(),
            warnings: self.report.warnings[before_warnings..].to_vec(),
        })
    }

    fn resolve_unit_result(
        &mut self,
        request: UnitResolveRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<Resolution<ResolvedUnit>, ResolverError> {
        self.resolve_unit_result_with_browsing(request, false, cancel)
    }

    fn resolve_unit_result_with_browsing(
        &mut self,
        request: UnitResolveRequest<'_>,
        skip_browsing: bool,
        cancel: &dyn CancellationToken,
    ) -> Result<Resolution<ResolvedUnit>, ResolverError> {
        self.check_cancel(cancel)?;
        let key_bytes = request.importer_path.as_os_str().len()
            + request.requested_name.len()
            + std::mem::size_of::<UnitCacheKey>();
        self.admit_lookup_storage(1, key_bytes, key_bytes, cancel)?;
        let key = UnitCacheKey::new(
            request.importer_path,
            request.requested_name,
            request.legacy_route,
        );
        if self.compile_time_store.is_none()
            && !skip_browsing
            && let Some(unit) = self.unit_cache.get(&key).cloned()
        {
            return Ok(Resolution::Found(unit));
        }
        let route = request.legacy_route.cloned();
        let result = self.resolve_unit_inner(request, skip_browsing, cancel)?;
        if matches!(&result, Resolution::Incomplete { .. }) {
            self.diagnostic_args(cancel, 30, format_args!("unit resolution was incomplete"))?;
        }
        if let Resolution::Found(unit) = &result {
            if self.compile_time_store.is_none() {
                self.unit_cache.insert(key, unit.clone());
            }
            if let Some(route) = self.legacy_route_for_source(&unit.source, route.as_ref()) {
                self.legacy_routes.insert(unit.source.id.clone(), route);
            }
        }
        Ok(result)
    }

    fn resolve_include_result(
        &mut self,
        request: IncludeResolveRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<Resolution<ResolvedSource>, ResolverError> {
        let result = self.resolve_include_inner(request, cancel)?;
        if matches!(&result, Resolution::Incomplete { .. }) {
            self.diagnostic_args(
                cancel,
                33,
                format_args!("include resolution was incomplete"),
            )?;
        }
        Ok(result)
    }

    fn check_cancel(&mut self, cancel: &dyn CancellationToken) -> Result<(), ResolverError> {
        if cancel.is_cancelled() {
            // Cancellation prevents new allocation; preserve scalar incomplete
            // state and all already admitted evidence without formatting first.
            self.report.complete = false;
            let _ = self.diagnostic_args(cancel, 17, format_args!("request cancelled"));
            Err(ResolverError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn resolve_unit_inner(
        &mut self,
        request: UnitResolveRequest<'_>,
        skip_browsing: bool,
        cancel: &dyn CancellationToken,
    ) -> Result<Resolution<ResolvedUnit>, ResolverError> {
        self.check_cancel(cancel)?;
        if self.owning_unit_reason {
            let count = self
                .context
                .unit_namespaces
                .len()
                .checked_add(self.context.search_path_entries.len())
                .and_then(|n| n.checked_add(self.context.browsing_path_entries.len()))
                .and_then(|n| n.checked_add(self.context.explicit_unit_entries.len()))
                .ok_or_else(|| {
                    ResolverError::InvalidRequest("unit lookup sizing work overflow".into())
                })?;
            let mut budget = self.store_budget(cancel).expect("owning unit reason root");
            budget
                .charge(count, 0, 0)
                .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
            for entries in self.context.explicit_unit_entries.values() {
                if cancel.is_cancelled() {
                    return Err(ResolverError::Cancelled);
                }
                budget
                    .charge(entries.len(), 0, 0)
                    .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
            }
        }
        let names = self
            .context
            .unit_namespaces
            .iter()
            .try_fold(0usize, |sum, n| {
                if cancel.is_cancelled() {
                    return Err(ResolverError::Cancelled);
                }
                sum.checked_add(n.len())
                    .and_then(|n| n.checked_add(std::mem::size_of::<String>()))
                    .ok_or_else(|| {
                        ResolverError::InvalidRequest("unit namespace sizing overflow".into())
                    })
            })?;
        let paths = self
            .context
            .search_path_entries
            .iter()
            .chain(&self.context.browsing_path_entries)
            .try_fold(0usize, |sum, e| {
                if cancel.is_cancelled() {
                    return Err(ResolverError::Cancelled);
                }
                sum.checked_add(e.path.as_os_str().len())
                    .and_then(|n| n.checked_add(std::mem::size_of::<ProjectPathEntry>()))
                    .ok_or_else(|| {
                        ResolverError::InvalidRequest("unit path sizing overflow".into())
                    })
            })?;
        let explicit = self
            .context
            .explicit_unit_entries
            .values()
            .flatten()
            .try_fold(0usize, |sum, e| {
                if cancel.is_cancelled() {
                    return Err(ResolverError::Cancelled);
                }
                e.path
                    .as_os_str()
                    .len()
                    .checked_mul(2)
                    .and_then(|n| n.checked_add(std::mem::size_of::<(PathBuf, ProjectPathEntry)>()))
                    .and_then(|n| sum.checked_add(n))
                    .ok_or_else(|| {
                        ResolverError::InvalidRequest("unit explicit sizing overflow".into())
                    })
            })?;
        let reserve = paths
            .saturating_mul(2)
            .saturating_add(explicit.saturating_mul(2))
            .saturating_add(names.saturating_mul(
                self.context.search_path_entries.len()
                    + self.context.browsing_path_entries.len()
                    + 2,
            ))
            .saturating_add(request.requested_name.len().saturating_mul(8));
        self.admit_lookup_storage(1, reserve, reserve, cancel)?;
        let requested = request.requested_name.trim();
        if requested.is_empty() {
            if self.owning_unit_reason {
                self.report.complete = false;
                self.diagnostic_args(cancel, 18, format_args!("unit name is empty"))?;
                return Ok(Resolution::Incomplete {
                    reason: self.unit_reason_build(
                        18,
                        cancel,
                        format_args!("unit name is empty"),
                    )?,
                    candidates: Vec::new(),
                });
            }
            return Err(ResolverError::InvalidRequest(
                "unit name is empty".to_string(),
            ));
        }
        if !self.context.can_resolve_units() {
            self.diagnostic_args(cancel, 30, format_args!("project metadata is incomplete"))?;
            return Ok(Resolution::Incomplete {
                reason: self.unit_reason_build(
                    30,
                    cancel,
                    format_args!("project metadata is incomplete"),
                )?,
                candidates: Vec::new(),
            });
        }
        let lookup = aliased_name(&self.context, requested);
        let missing_issue = if self.owning_unit_reason {
            let mut budget = self.store_budget(cancel).expect("owning unit reason root");
            budget
                .charge(self.context.path_issues.len(), 0, 0)
                .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
            let mut found = None;
            for issue in &self.context.path_issues {
                if cancel.is_cancelled() {
                    return Err(ResolverError::Cancelled);
                }
                if issue.kind == pascal_project::ProjectPathIssueKind::MissingReference {
                    if let Some(name) = &issue.unit_name {
                        budget
                            .charge(1, name.len().min(lookup.len()), 0)
                            .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
                        if name.eq_ignore_ascii_case(&lookup) {
                            found = Some(issue);
                            break;
                        }
                    }
                }
            }
            found
        } else {
            self.context.missing_explicit_unit(&lookup)
        };
        if let Some(issue) = missing_issue {
            let reason = if self.owning_unit_reason {
                let mut budget = self.store_budget(cancel).expect("owning unit reason root");
                let bound = lookup
                    .len()
                    .checked_add(64)
                    .and_then(|n| {
                        n.checked_add(
                            issue
                                .path
                                .as_ref()
                                .map_or(Some(0), |p| p.as_os_str().len().checked_mul(4))?,
                        )
                    })
                    .ok_or_else(|| {
                        ResolverError::InvalidRequest("explicit unit reason bound overflow".into())
                    })?;
                let owner = if let Some(path) = &issue.path {
                    build_unit_reason(
                        &mut budget,
                        bound,
                        cancel,
                        format_args!("explicit unit {lookup} is missing: {}", path.display()),
                    )?
                } else {
                    build_unit_reason(
                        &mut budget,
                        bound,
                        cancel,
                        format_args!("explicit unit {lookup} is missing"),
                    )?
                };
                if self.callback_unit_reason.is_some() {
                    return Err(ResolverError::InvalidRequest(
                        "duplicate unit reason construction".into(),
                    ));
                }
                self.callback_unit_reason = Some(owner);
                String::new()
            } else {
                issue.path.as_ref().map_or_else(
                    || format!("explicit unit {lookup} is missing"),
                    |path| format!("explicit unit {lookup} is missing: {}", path.display()),
                )
            };
            return Ok(Resolution::Incomplete {
                reason,
                candidates: Vec::new(),
            });
        }
        if let Some(entries) = self
            .context
            .explicit_unit_entries
            .get(&lookup.to_ascii_lowercase())
        {
            let group = CandidateGroup {
                paths: self.explicit_structured_paths(entries, cancel)?,
                legacy_route: request.legacy_route,
            };
            if let Some(result) =
                self.resolve_candidate_group(&group, requested, &lookup, cancel)?
            {
                return Ok(result);
            }
        } else if let Some(paths) = self
            .context
            .explicit_units
            .get(&lookup.to_ascii_lowercase())
        {
            let paths: Vec<_> = paths
                .iter()
                .filter_map(|path| {
                    self.context
                        .path_entry_for(path)
                        .or_else(|| self.legacy_entry_for_path(path, request.legacy_route))
                        .map(|entry| (path.clone(), entry))
                })
                .collect();
            let group = CandidateGroup {
                paths: paths.into(),
                legacy_route: request.legacy_route,
            };
            if let Some(result) =
                self.resolve_candidate_group(&group, requested, &lookup, cancel)?
            {
                return Ok(result);
            }
        }

        if let Some(directory) = request.importer_path.parent()
            && let Some(result) = self.resolve_directory(
                directory,
                &lookup,
                &self.context.unit_namespaces.clone(),
                request.legacy_route,
                cancel,
                None,
                requested,
            )?
        {
            return Ok(result);
        }
        let search_entries = if self.context.search_path_entries.is_empty() {
            self.context
                .search_paths
                .iter()
                .filter_map(|path| self.context.path_entry_for(path))
                .collect::<Vec<_>>()
        } else {
            self.context.search_path_entries.clone()
        };
        for entry in search_entries {
            if let Some(result) = self.resolve_directory(
                &entry.path,
                &lookup,
                &self.context.unit_namespaces.clone(),
                request.legacy_route,
                cancel,
                Some(&entry),
                requested,
            )? {
                return Ok(result);
            }
        }

        // Browsing paths are a positive source-navigation fallback only. They
        // follow every project/client and selected-installation library path,
        // and missing explicit bindings were reserved above.
        if !skip_browsing {
            for entry in self.context.browsing_path_entries.clone() {
                if let Some(result) = self.resolve_directory(
                    &entry.path,
                    &lookup,
                    &self.context.unit_namespaces.clone(),
                    request.legacy_route,
                    cancel,
                    Some(&entry),
                    requested,
                )? {
                    return Ok(result);
                }
            }
        }

        if self.context.project_file.is_none() {
            let catalogue_groups = self.filename_catalogue_candidates(&lookup, cancel)?;
            for group in catalogue_groups {
                if let Some(result) =
                    self.resolve_candidate_group(&group, requested, &lookup, cancel)?
                {
                    return Ok(result);
                }
            }
        }

        if !self.context.packages.is_empty() {
            self.package_lookup_for_unit(requested, &lookup, request.legacy_route, cancel)
        } else {
            Ok(Resolution::Unavailable {
                reason: self.unit_reason_build(
                    requested.len().checked_add(40).ok_or_else(|| {
                        ResolverError::InvalidRequest("unit reason bound overflow".into())
                    })?,
                    cancel,
                    format_args!("no authorized unit candidate for {requested}"),
                )?,
            })
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_directory(
        &mut self,
        directory: &Path,
        unit_name: &str,
        namespaces: &[String],
        legacy_route: Option<&LegacyRoute>,
        cancel: &dyn CancellationToken,
        provided_entry: Option<&ProjectPathEntry>,
        requested: &str,
    ) -> Result<Option<Resolution<ResolvedUnit>>, ResolverError> {
        let bytes = directory.as_os_str().len().saturating_mul(8); // canonical directory/entry scratch remains a later boundary
        self.admit_lookup_storage(1, bytes, bytes, cancel)?;
        let directory = canonical_path(directory);
        let Some(entry) = provided_entry
            .cloned()
            .or_else(|| self.context.path_entry_for(&directory))
            .or_else(|| self.legacy_entry_for_directory(&directory, legacy_route))
        else {
            return Ok(None);
        };
        let entry = ProjectPathEntry {
            path: directory.clone(),
            provenance: entry.provenance,
        };
        let listing = self.list_directory(&directory, &entry, legacy_route, cancel, false)?;
        if !listing.complete {
            return Ok(Some(Resolution::Incomplete {
                reason: self.unit_reason_build(
                    directory
                        .as_os_str()
                        .len()
                        .checked_mul(4)
                        .and_then(|n| n.checked_add(64))
                        .ok_or_else(|| {
                            ResolverError::InvalidRequest(
                                "directory unit reason bound overflow".into(),
                            )
                        })?,
                    cancel,
                    format_args!(
                        "directory scan under {} was incomplete",
                        directory.display()
                    ),
                )?,
                candidates: Vec::new(),
            }));
        }
        let tiers = self.filename_tiers_scratch(unit_name, namespaces, cancel)?;
        for names in tiers.iter() {
            let paths = self.paths_for_names(&directory, &entry, &names, &listing, cancel)?;
            let group = CandidateGroup {
                paths,
                legacy_route,
            };
            if let Some(result) =
                self.resolve_candidate_group(&group, requested, unit_name, cancel)?
            {
                return Ok(Some(result));
            }
        }
        if unit_name.contains('.') {
            let parts = unit_name
                .split('.')
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>();
            if parts.len() > 1 {
                let nested = parts[..parts.len() - 1]
                    .iter()
                    .fold(directory.clone(), |path, part| path.join(part));
                let nested = match self.resolve_case_insensitive_path(
                    &directory,
                    &nested,
                    &entry,
                    legacy_route,
                    cancel,
                )? {
                    CaseInsensitiveLookup::Found(nested) => Some(nested),
                    CaseInsensitiveLookup::Missing => None,
                    CaseInsensitiveLookup::Ambiguous(paths) => {
                        if self.owning_unit_candidates {
                            let mut budget = self
                                .store_budget(cancel)
                                .expect("owning unit candidate root");
                            budget
                                .charge(paths.len(), 0, 0)
                                .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
                            let mut candidates = candidate_vector(paths.len(), &mut budget)?;
                            for path in paths.iter() {
                                if cancel.is_cancelled() {
                                    return Err(ResolverError::Cancelled);
                                }
                                let candidate =
                                    unit_candidate_entry(path, None, &mut budget, cancel)?;
                                push_unit_candidate(&mut candidates, candidate, &budget)?;
                            }
                            return self.candidate_result(candidates, None);
                        }
                        // The plain legacy/result contract is still backed by
                        // its unmigrated result invoice. Borrow the new match
                        // owner and admit this independent receiving copy;
                        // never extract paths and refund live plain metadata.
                        let bytes = case_result_payload_size(
                            &paths,
                            self.store_budget(cancel).as_mut(),
                            cancel,
                        )?;
                        let retained = paths
                            .len()
                            .checked_mul(std::mem::size_of::<ResolutionCandidate>())
                            .and_then(|n| n.checked_add(bytes))
                            .ok_or_else(|| {
                                ResolverError::InvalidRequest("case result backing overflow".into())
                            })?;
                        self.admit_lookup_storage(paths.len(), bytes, retained, cancel)?;
                        return Ok(Some(Resolution::Ambiguous {
                            candidates: paths
                                .iter()
                                .map(|path| ResolutionCandidate {
                                    path: path.clone(),
                                    declared_name: None,
                                })
                                .collect(),
                        }));
                    }
                    CaseInsensitiveLookup::Incomplete => {
                        return Ok(Some(Resolution::Incomplete {
                            reason: self.unit_reason_build(
                                48,
                                cancel,
                                format_args!("case-insensitive path lookup was incomplete"),
                            )?,
                            candidates: Vec::new(),
                        }));
                    }
                };
                if let Some(nested) = nested {
                    let nested_entry = self.candidate_entry_for_path(&nested, &entry, None);
                    let nested_listing =
                        self.list_directory(&nested, &nested_entry, legacy_route, cancel, false)?;
                    if !nested_listing.complete {
                        return Ok(Some(Resolution::Incomplete {
                            reason: self.unit_reason_build(
                                nested
                                    .as_os_str()
                                    .len()
                                    .checked_mul(4)
                                    .and_then(|n| n.checked_add(64))
                                    .ok_or_else(|| {
                                        ResolverError::InvalidRequest(
                                            "nested directory unit reason bound overflow".into(),
                                        )
                                    })?,
                                cancel,
                                format_args!(
                                    "directory scan under {} was incomplete",
                                    nested.display()
                                ),
                            )?,
                            candidates: Vec::new(),
                        }));
                    }
                    let nested_tiers = self.filename_tiers_scratch(
                        parts.last().copied().unwrap_or_default(),
                        &[],
                        cancel,
                    )?;
                    for names in nested_tiers.iter() {
                        let group = CandidateGroup {
                            paths: self.paths_for_names(
                                &nested,
                                &nested_entry,
                                &names,
                                &nested_listing,
                                cancel,
                            )?,
                            legacy_route,
                        };
                        if let Some(result) =
                            self.resolve_candidate_group(&group, requested, unit_name, cancel)?
                        {
                            return Ok(Some(result));
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    fn explicit_structured_paths(
        &self,
        entries: &[ProjectPathEntry],
        cancel: &dyn CancellationToken,
    ) -> Result<LookupScratch<Vec<(PathBuf, ProjectPathEntry)>>, ResolverError> {
        let Some(mut budget) = self.store_budget(cancel) else {
            return Ok(LookupScratch::Legacy(
                entries
                    .iter()
                    .map(|entry| (entry.path.clone(), entry.clone()))
                    .collect(),
            ));
        };
        let mut result = explicit_path_vector(entries.len(), &mut budget)?;
        budget
            .charge(entries.len(), 0, 0)
            .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
        for entry in entries {
            if cancel.is_cancelled() {
                return Err(ResolverError::Cancelled);
            }
            let local = source_request_entry(&entry.path, entry, &mut budget, cancel)
                .map_err(root_store_error)?;
            explicit_path_push(&mut result, &entry.path, local, &mut budget)?;
        }
        Ok(LookupScratch::Admitted(result))
    }

    fn legacy_entry_for_path(
        &self,
        path: &Path,
        route: Option<&LegacyRoute>,
    ) -> Option<ProjectPathEntry> {
        route
            .filter(|route| legacy_route_authorizes(path, route))
            .map(|_| ProjectPathEntry::legacy(canonical_path(path)))
    }

    fn legacy_route_for_source(
        &self,
        source: &LoadedSource,
        inherited: Option<&LegacyRoute>,
    ) -> Option<LegacyRoute> {
        let source_is_legacy = match &source.revision {
            SourceRevision::Disk { path_entry, .. } => {
                matches!(path_entry.provenance, ProjectPathProvenance::LegacyNative)
            }
            SourceRevision::Overlay { .. } => {
                self.context
                    .path_entry_for(&source.path)
                    .is_some_and(|entry| {
                        matches!(entry.provenance, ProjectPathProvenance::LegacyNative)
                    })
                    || inherited.is_some_and(|route| legacy_route_authorizes(&source.path, route))
            }
        };
        source_is_legacy.then(|| {
            let sibling_directory = source.path.parent().unwrap_or(&source.path).to_path_buf();
            LegacyRoute {
                source_path: source.path.clone(),
                sibling_directory,
            }
        })
    }

    fn legacy_entry_for_directory(
        &self,
        directory: &Path,
        route: Option<&LegacyRoute>,
    ) -> Option<ProjectPathEntry> {
        route
            .filter(|route| legacy_route_authorizes_directory(directory, route))
            .map(|_| ProjectPathEntry::legacy(canonical_path(directory)))
    }

    fn candidate_entry_for_path(
        &self,
        path: &Path,
        inherited: &ProjectPathEntry,
        preferred_provenance: Option<&ProjectPathProvenance>,
    ) -> ProjectPathEntry {
        let path = canonical_path(path);
        if let Some(entry) = self.exact_context_entry_for_path(&path) {
            return ProjectPathEntry {
                path,
                provenance: entry.provenance,
            };
        }
        ProjectPathEntry {
            path,
            provenance: preferred_provenance
                .cloned()
                .unwrap_or_else(|| inherited.provenance.clone()),
        }
    }

    fn exact_context_entry_for_path(&self, path: &Path) -> Option<ProjectPathEntry> {
        self.context
            .main_source_entry
            .iter()
            .chain(self.context.explicit_unit_entries.values().flatten())
            .find(|entry| path_equivalent(&entry.path, path))
            .cloned()
    }

    fn resolve_candidate_group(
        &mut self,
        group: &CandidateGroup,
        requested: &str,
        lookup: &str,
        cancel: &dyn CancellationToken,
    ) -> Result<Option<Resolution<ResolvedUnit>>, ResolverError> {
        if let (LookupScratch::Admitted(paths), Some(budget)) =
            (&group.paths, self.store_budget(cancel))
        {
            if !paths.is_admitted_by(&budget) {
                return Err(ResolverError::InvalidRequest(
                    "foreign-root candidate group".into(),
                ));
            }
        }
        if self.owning_unit_candidates {
            return self.resolve_candidate_group_owned(group, requested, lookup, cancel);
        }
        self.check_cancel(cancel)?;
        if self.owning_unit_callback {
            let mut budget = self.store_budget(cancel).expect("owning unit root");
            budget
                .charge(group.paths.len(), 0, 0)
                .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
        }
        let copies = group.paths.iter().try_fold(0usize, |sum, (p, e)| {
            if cancel.is_cancelled() {
                return Err(ResolverError::Cancelled);
            }
            p.as_os_str()
                .len()
                .checked_add(e.path.as_os_str().len())
                .and_then(|n| n.checked_add(std::mem::size_of::<ResolutionCandidate>()))
                .and_then(|n| n.checked_mul(8))
                .and_then(|n| sum.checked_add(n))
                .ok_or_else(|| {
                    ResolverError::InvalidRequest("candidate construction bound overflow".into())
                })
        })?;
        self.admit_lookup_storage(1, copies, copies, cancel)?;
        type ValidCandidate = (
            LoadedSource,
            ResolutionCandidate,
            Option<AdmittedOwned<String>>,
        );
        let _valid_capacity = if self.owning_unit_callback {
            let mut budget = self.store_budget(cancel).expect("owning unit root");
            let bytes = group
                .paths
                .len()
                .checked_mul(std::mem::size_of::<ValidCandidate>())
                .ok_or_else(|| {
                    ResolverError::InvalidRequest("selected candidate capacity overflow".into())
                })?;
            Some(
                budget
                    .admit_owned(1, 0, bytes, || ())
                    .map_err(|r| crate::compile_time::resolver_error(r, &budget))?,
            )
        } else {
            None
        };
        let mut valid = if self.owning_unit_callback {
            Vec::with_capacity(group.paths.len())
        } else {
            Vec::new()
        };
        let mut candidates = Vec::new();
        let mut order_bound = group
            .paths
            .len()
            .checked_mul(std::mem::size_of::<usize>() * 2)
            .ok_or_else(|| ResolverError::InvalidRequest("candidate ordering overflow".into()))?;
        if self.owning_unit_callback {
            let mut budget = self.store_budget(cancel).expect("owning unit root");
            budget
                .charge(group.paths.len(), 0, 0)
                .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
        }
        for (path, _) in group.paths.iter() {
            if cancel.is_cancelled() {
                return Err(ResolverError::Cancelled);
            }
            order_bound = order_bound
                .checked_add(
                    path.as_os_str()
                        .len()
                        .checked_mul(8)
                        .and_then(|n| n.checked_add(32))
                        .ok_or_else(|| {
                            ResolverError::InvalidRequest("candidate ordering key overflow".into())
                        })?,
                )
                .ok_or_else(|| {
                    ResolverError::InvalidRequest("candidate ordering bound overflow".into())
                })?;
        }
        let build_order = || {
            let mut order = (0..group.paths.len())
                .filter(|index| {
                    !group.paths[..*index]
                        .iter()
                        .any(|(previous, _)| path_equivalent(previous, &group.paths[*index].0))
                })
                .collect::<Vec<_>>();
            order.sort_by_key(|index| path_key(&group.paths[*index].0));
            order
        };
        let order = if let Some(mut budget) = self.store_budget(cancel) {
            let work = group
                .paths
                .len()
                .checked_mul(group.paths.len())
                .ok_or_else(|| {
                    ResolverError::InvalidRequest("candidate ordering work overflow".into())
                })?;
            LookupScratch::Admitted(
                budget
                    .admit_owned(work, order_bound, order_bound, build_order)
                    .map_err(|r| crate::compile_time::resolver_error(r, &budget))?,
            )
        } else {
            LookupScratch::Legacy(build_order())
        };
        for index in order.iter() {
            let (path, entry) = &group.paths[*index];
            self.check_cancel(cancel)?;
            match self.load_candidate(&path, &entry, group.legacy_route, SourceKind::Unit, cancel) {
                Ok(source) => {
                    let mut parsed_owner = if self.owning_unit_callback {
                        let mut budget = self.store_budget(cancel).expect("owning unit root");
                        owned_header_name(source.analysis_bytes().as_ref(), &mut budget, cancel)?
                    } else {
                        None
                    };
                    let declared_name = if self.owning_unit_callback {
                        None
                    } else if self.compile_time_store.is_some() {
                        let bytes = source.analysis_bytes();
                        self.admit_lookup_storage(bytes.len(), bytes.len(), 0, cancel)?;
                        if let Some(name) = admitted_header_name(&bytes) {
                            self.admit_lookup_storage(
                                1,
                                name.len(),
                                2 * name.len() + std::mem::size_of::<String>(),
                                cancel,
                            )?;
                            Some(canonical_unit_name(name))
                        } else {
                            None
                        }
                    } else {
                        match self.store.declared_unit_name(&source) {
                            Some(declared_name) => declared_name,
                            None => self.source_metadata(&source).declared_name.clone(),
                        }
                    };
                    let valid_name = declared_name_matches(
                        parsed_owner
                            .as_ref()
                            .map(|n| n.as_str())
                            .or(declared_name.as_deref()),
                        requested,
                        lookup,
                        &self.context,
                    );
                    // Only unselected/ambiguous legacy candidate metadata gets
                    // its separate bounded copy. Selected names keep the
                    // original header owner, never a clone of that candidate.
                    let candidate_name = if self.owning_unit_callback && !valid_name {
                        if let Some(name) = parsed_owner.as_ref() {
                            self.admit_lookup_storage(1, name.len(), name.len(), cancel)?;
                            Some((**name).clone())
                        } else {
                            None
                        }
                    } else {
                        declared_name.clone()
                    };
                    let candidate = ResolutionCandidate {
                        path: source.path.clone(),
                        declared_name: candidate_name,
                    };
                    if valid_name {
                        valid.push((source, candidate, parsed_owner.take()));
                    } else {
                        candidates.push(candidate);
                    }
                }
                Err(SourceStoreError::NotFound { path: missing }) => {
                    self.record_candidate(&missing, Some(&entry), false, cancel)?;
                }
                Err(SourceStoreError::Unauthorized { path, reason }) => {
                    self.warning_args(
                        cancel,
                        128 + 4 * path.as_os_str().len() + reason.len(),
                        format_args!(
                            "unit candidate {} is unauthorized: {reason}",
                            path.display()
                        ),
                    )?;
                    self.diagnostic_args(
                        cancel,
                        64 + 4 * path.as_os_str().len(),
                        format_args!("unit candidate {} is unauthorized", path.display()),
                    )?;
                    return Ok(Some(Resolution::Incomplete {
                        reason: self.unit_reason_build(
                            32,
                            cancel,
                            format_args!("unit candidate is unauthorized"),
                        )?,
                        candidates,
                    }));
                }
                Err(SourceStoreError::Cancelled) => return Err(ResolverError::Cancelled),
                Err(error @ SourceStoreError::RootLimit { .. }) => {
                    return Err(root_store_error(error));
                }
                Err(SourceStoreError::Incomplete { path, reason }) => {
                    self.diagnostic_args(
                        cancel,
                        128 + 4 * path.as_os_str().len() + reason.len(),
                        format_args!("unit candidate {} is incomplete: {reason}", path.display()),
                    )?;
                    let reason = if self.owning_unit_reason {
                        self.unit_reason_build(reason.len(), cancel, format_args!("{reason}"))?
                    } else {
                        reason
                    };
                    return Ok(Some(Resolution::Incomplete { reason, candidates }));
                }
                Err(error) => {
                    self.diagnostic_args(
                        cancel,
                        source_error_report_bound(&error),
                        format_args!("unit candidate could not be loaded: {error:?}"),
                    )?;
                    let reason = self.unit_reason_build(
                        source_error_report_bound(&error),
                        cancel,
                        format_args!("unit candidate could not be loaded: {error:?}"),
                    )?;
                    return Ok(Some(Resolution::Incomplete { reason, candidates }));
                }
            }
        }
        match valid.len() {
            0 => Ok(None),
            1 => {
                let (source, candidate, declared_owner) = valid.pop().expect("one valid candidate");
                if self.owning_unit_callback {
                    let mut budget = self.store_budget(cancel).expect("owning unit root");
                    let requested = budget
                        .admit_owned(1, requested.len(), requested.len(), || {
                            requested.to_string()
                        })
                        .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
                    let declared = match declared_owner {
                        Some(name) => name,
                        None => budget
                            .admit_owned(1, lookup.len(), lookup.len(), || lookup.to_string())
                            .map_err(|r| crate::compile_time::resolver_error(r, &budget))?,
                    };
                    if self.callback_unit_names.is_some() {
                        return Err(ResolverError::InvalidRequest(
                            "duplicate selected unit names".into(),
                        ));
                    }
                    self.callback_unit_names = Some(OwnedUnitNames {
                        requested,
                        declared,
                    });
                    return Ok(Some(Resolution::Found(ResolvedUnit {
                        requested_name: String::new(),
                        declared_name: String::new(),
                        source,
                    })));
                }
                Ok(Some(Resolution::Found(ResolvedUnit {
                    requested_name: requested.to_string(),
                    declared_name: candidate
                        .declared_name
                        .unwrap_or_else(|| lookup.to_string()),
                    source,
                })))
            }
            _ => {
                valid.sort_by_key(|left| path_key(&left.0.path));
                let mut candidates = Vec::with_capacity(valid.len());
                for (_, mut candidate, name) in valid {
                    if let Some(name) = name {
                        self.admit_lookup_storage(1, name.len(), name.len(), cancel)?;
                        candidate.declared_name = Some((*name).clone());
                    }
                    candidates.push(candidate);
                }
                self.warning_args(
                    cancel,
                    64 + requested.len(),
                    format_args!("ambiguous unit {requested}: multiple valid candidates"),
                )?;
                Ok(Some(Resolution::Ambiguous { candidates }))
            }
        }
    }

    fn candidate_result(
        &mut self,
        candidates: AdmittedOwned<Vec<ResolutionCandidate>>,
        reason: Option<String>,
    ) -> Result<Option<Resolution<ResolvedUnit>>, ResolverError> {
        if self.callback_unit_candidates.is_some() {
            return Err(ResolverError::InvalidRequest(
                "duplicate unit candidate construction".into(),
            ));
        }
        self.callback_unit_candidates = Some(candidates);
        Ok(Some(match reason {
            Some(reason) => Resolution::Incomplete {
                reason,
                candidates: Vec::new(),
            },
            None => Resolution::Ambiguous {
                candidates: Vec::new(),
            },
        }))
    }

    fn resolve_candidate_group_owned(
        &mut self,
        group: &CandidateGroup,
        requested: &str,
        lookup: &str,
        cancel: &dyn CancellationToken,
    ) -> Result<Option<Resolution<ResolvedUnit>>, ResolverError> {
        self.check_cancel(cancel)?;
        let overflow =
            || ResolverError::InvalidRequest("owned candidate construction bound overflow".into());
        let count = group.paths.len();
        let mut budget = self
            .store_budget(cancel)
            .expect("owning unit candidates root");
        budget
            .charge(count, 0, 0)
            .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
        // This old conservative invoice also covers source/entry/group scratch.
        // Do not carve candidate payload lengths out of that mixed bound.
        let mut copies = 0usize;
        let mut order_bound = count
            .checked_mul(2 * std::mem::size_of::<usize>())
            .ok_or_else(overflow)?;
        for (path, entry) in group.paths.iter() {
            if cancel.is_cancelled() {
                return Err(ResolverError::Cancelled);
            }
            let copy = path
                .as_os_str()
                .len()
                .checked_add(entry.path.as_os_str().len())
                .and_then(|n| n.checked_add(std::mem::size_of::<ResolutionCandidate>()))
                .and_then(|n| n.checked_mul(8))
                .ok_or_else(overflow)?;
            copies = copies.checked_add(copy).ok_or_else(overflow)?;
            order_bound = order_bound
                .checked_add(
                    path.as_os_str()
                        .len()
                        .checked_mul(8)
                        .and_then(|n| n.checked_add(32))
                        .ok_or_else(overflow)?,
                )
                .ok_or_else(overflow)?;
        }
        self.admit_lookup_storage(1, copies, copies, cancel)?;
        type Valid = (LoadedSource, Option<AdmittedOwned<String>>);
        let mut valid = budget
            .admit_owned(
                1,
                0,
                count
                    .checked_mul(std::mem::size_of::<Valid>())
                    .ok_or_else(overflow)?,
                || Vec::<Valid>::with_capacity(count),
            )
            .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
        let mut rejected = candidate_vector(count, &mut budget)?;
        // Index dedup, sort-key allocations and backing have their own temporary
        // invoice; stable source-path ordering is already established here.
        let order = budget.admit_fallible(
            count.checked_mul(count).ok_or_else(overflow)?,
            order_bound,
            order_bound,
            crate::compile_time::resolver_error,
            || {
                let mut order = Vec::with_capacity(count);
                for index in 0..count {
                    if cancel.is_cancelled() {
                        return Err(ResolverError::Cancelled);
                    }
                    if !group.paths[..index]
                        .iter()
                        .any(|(previous, _)| path_equivalent(previous, &group.paths[index].0))
                    {
                        order.push(index);
                    }
                }
                order.sort_by_key(|index| path_key(&group.paths[*index].0));
                if cancel.is_cancelled() {
                    return Err(ResolverError::Cancelled);
                }
                Ok(order)
            },
        )?;
        for index in order.iter() {
            let (path, entry) = &group.paths[*index];
            self.check_cancel(cancel)?;
            match self.load_candidate(path, entry, group.legacy_route, SourceKind::Unit, cancel) {
                Ok(source) => {
                    let name =
                        owned_header_name(source.analysis_bytes().as_ref(), &mut budget, cancel)?;
                    if declared_name_matches_owned(
                        name.as_ref().map(|n| n.as_str()),
                        requested,
                        lookup,
                        &self.context,
                        &mut budget,
                        cancel,
                    )? {
                        valid.value_mut().push((source, name));
                    } else {
                        let candidate =
                            unit_candidate_entry(&source.path, name, &mut budget, cancel)?;
                        push_unit_candidate(&mut rejected, candidate, &budget)?;
                    }
                }
                Err(SourceStoreError::NotFound { path: missing }) => {
                    self.record_candidate(&missing, Some(entry), false, cancel)?
                }
                Err(SourceStoreError::Cancelled) => return Err(ResolverError::Cancelled),
                Err(error @ SourceStoreError::RootLimit { .. }) => {
                    return Err(root_store_error(error));
                }
                Err(SourceStoreError::Unauthorized { path, reason }) => {
                    self.warning_args(
                        cancel,
                        128 + 4 * path.as_os_str().len() + reason.len(),
                        format_args!(
                            "unit candidate {} is unauthorized: {reason}",
                            path.display()
                        ),
                    )?;
                    self.diagnostic_args(
                        cancel,
                        64 + 4 * path.as_os_str().len(),
                        format_args!("unit candidate {} is unauthorized", path.display()),
                    )?;
                    let reason = self.unit_reason_build(
                        32,
                        cancel,
                        format_args!("unit candidate is unauthorized"),
                    )?;
                    return self.candidate_result(rejected, Some(reason));
                }
                Err(SourceStoreError::Incomplete { path, reason }) => {
                    self.diagnostic_args(
                        cancel,
                        128 + 4 * path.as_os_str().len() + reason.len(),
                        format_args!("unit candidate {} is incomplete: {reason}", path.display()),
                    )?;
                    let reason =
                        self.unit_reason_build(reason.len(), cancel, format_args!("{reason}"))?;
                    return self.candidate_result(rejected, Some(reason));
                }
                Err(error) => {
                    self.diagnostic_args(
                        cancel,
                        source_error_report_bound(&error),
                        format_args!("unit candidate could not be loaded: {error:?}"),
                    )?;
                    let reason = self.unit_reason_build(
                        source_error_report_bound(&error),
                        cancel,
                        format_args!("unit candidate could not be loaded: {error:?}"),
                    )?;
                    return self.candidate_result(rejected, Some(reason));
                }
            }
        }
        match valid.len() {
            0 => Ok(None),
            1 => {
                let (source, name) = valid.value_mut().pop().expect("one valid owned candidate");
                let requested = budget
                    .admit_owned(1, requested.len(), requested.len(), || {
                        requested.to_string()
                    })
                    .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
                let declared = match name {
                    Some(name) => name,
                    None => budget
                        .admit_owned(1, lookup.len(), lookup.len(), || lookup.to_string())
                        .map_err(|r| crate::compile_time::resolver_error(r, &budget))?,
                };
                if self.callback_unit_names.is_some() {
                    return Err(ResolverError::InvalidRequest(
                        "duplicate selected unit names".into(),
                    ));
                }
                self.callback_unit_names = Some(OwnedUnitNames {
                    requested,
                    declared,
                });
                Ok(Some(Resolution::Found(ResolvedUnit {
                    requested_name: String::new(),
                    declared_name: String::new(),
                    source,
                })))
            }
            _ => {
                // Loaded source spellings can differ from requested paths.
                // Preserve the legacy canonical-source ordering, admitting its
                // key/sort scratch separately from all escaped vector fields.
                budget
                    .charge(valid.len(), 0, 0)
                    .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
                let mut keys = 0usize;
                for (source, _) in valid.iter() {
                    if cancel.is_cancelled() {
                        return Err(ResolverError::Cancelled);
                    }
                    keys = keys
                        .checked_add(
                            source
                                .path
                                .as_os_str()
                                .len()
                                .checked_mul(8)
                                .and_then(|n| n.checked_add(32))
                                .ok_or_else(overflow)?,
                        )
                        .ok_or_else(overflow)?;
                }
                let _sort = budget
                    .admit_owned(
                        valid.len().checked_mul(valid.len()).ok_or_else(overflow)?,
                        keys.checked_mul(valid.len()).ok_or_else(overflow)?,
                        keys.checked_add(
                            valid
                                .len()
                                .checked_mul(std::mem::size_of::<Valid>())
                                .ok_or_else(overflow)?,
                        )
                        .ok_or_else(overflow)?,
                        || (),
                    )
                    .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
                valid
                    .value_mut()
                    .sort_by_key(|(source, _)| path_key(&source.path));
                if cancel.is_cancelled() {
                    return Err(ResolverError::Cancelled);
                }
                let mut candidates = candidate_vector(valid.len(), &mut budget)?;
                // Keep the actual valid tuple allocation charged while draining;
                // don't extract its live Vec and retire its capacity first.
                for (source, name) in valid.value_mut().drain(..) {
                    if cancel.is_cancelled() {
                        return Err(ResolverError::Cancelled);
                    }
                    let candidate = unit_candidate_entry(&source.path, name, &mut budget, cancel)?;
                    push_unit_candidate(&mut candidates, candidate, &budget)?;
                }
                self.warning_args(
                    cancel,
                    64 + requested.len(),
                    format_args!("ambiguous unit {requested}: multiple valid candidates"),
                )?;
                self.candidate_result(candidates, None)
            }
        }
    }

    fn paths_for_names(
        &mut self,
        directory: &Path,
        entry: &ProjectPathEntry,
        names: &[String],
        listing: &DirectoryListing,
        cancel: &dyn CancellationToken,
    ) -> Result<LookupScratch<Vec<(PathBuf, ProjectPathEntry)>>, ResolverError> {
        if self.compile_time_store.is_some() {
            return self.paths_for_names_owned(directory, entry, names, listing, cancel);
        }
        let copy_bound = listing
            .files
            .iter()
            .map(|p| {
                6 * p.as_os_str().len() + 4 * std::mem::size_of::<(PathBuf, ProjectPathEntry)>()
            })
            .sum::<usize>();
        self.admit_lookup_storage(1, copy_bound, copy_bound, cancel)?;
        let mut paths = listing
            .files
            .iter()
            .filter(|path| {
                path.file_name().is_some_and(|file_name| {
                    names
                        .iter()
                        .any(|name| file_name.to_string_lossy().eq_ignore_ascii_case(name))
                })
            })
            .map(|path| {
                (
                    path.clone(),
                    self.candidate_entry_for_path(path, entry, None),
                )
            })
            .collect::<Vec<_>>();
        let admitted = if let Some(mut budget) = self.store_budget(cancel) {
            Some(
                self.store
                    .overlay_candidates_admitted(
                        &[directory.to_path_buf()],
                        names,
                        &mut budget,
                        cancel,
                    )
                    .map_err(root_store_error)?,
            )
        } else {
            None
        };
        if let Some(ref admitted) = admitted {
            let bytes = admitted
                .iter()
                .map(|p| {
                    6 * p.as_os_str().len() + 4 * std::mem::size_of::<(PathBuf, ProjectPathEntry)>()
                })
                .sum();
            self.admit_lookup_storage(1, bytes, bytes, cancel)?;
        }
        let overlays = if let Some(ref admitted) = admitted {
            admitted.to_vec()
        } else {
            self.store
                .overlay_candidates(&[directory.to_path_buf()], names)
        };
        for path in overlays {
            if path
                .parent()
                .is_none_or(|parent| !path_equivalent(parent, directory))
            {
                continue;
            }
            if !paths
                .iter()
                .any(|(existing, _)| path_equivalent(existing, &path))
            {
                paths.push((
                    path.clone(),
                    self.candidate_entry_for_path(&path, entry, None),
                ));
            }
        }
        paths.sort_by_key(|left| path_key(&left.0));
        Ok(LookupScratch::Legacy(paths))
    }

    fn filename_tiers_scratch(
        &self,
        unit_name: &str,
        namespaces: &[String],
        cancel: &dyn CancellationToken,
    ) -> Result<LookupScratch<Vec<Vec<String>>>, ResolverError> {
        let Some(mut budget) = self.store_budget(cancel) else {
            return Ok(filename_tiers(unit_name, namespaces).into());
        };
        let overflow =
            || ResolverError::InvalidRequest("filename tier construction overflow".into());
        let count = namespaces.len().checked_add(1).ok_or_else(overflow)?;
        let mut bytes = count
            .checked_mul(4 * std::mem::size_of::<Vec<String>>() + 2 * std::mem::size_of::<String>())
            .ok_or_else(overflow)?;
        let exact = unit_name
            .len()
            .checked_add(4)
            .and_then(|n| n.checked_mul(4))
            .map(|n| n.max(32))
            .ok_or_else(overflow)?;
        bytes = bytes.checked_add(exact).ok_or_else(overflow)?;
        for namespace in namespaces {
            bytes = bytes
                .checked_add(
                    namespace
                        .len()
                        .checked_add(unit_name.len())
                        .and_then(|n| n.checked_add(5))
                        .and_then(|n| n.checked_mul(2))
                        .map(|n| n.max(32))
                        .ok_or_else(overflow)?,
                )
                .ok_or_else(overflow)?;
        }
        let tiers = budget
            .admit_owned(count, bytes, bytes, || {
                filename_tiers(unit_name, namespaces)
            })
            .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
        Ok(LookupScratch::Admitted(tiers))
    }

    fn paths_for_names_owned(
        &mut self,
        directory: &Path,
        entry: &ProjectPathEntry,
        names: &[String],
        listing: &DirectoryListing,
        cancel: &dyn CancellationToken,
    ) -> Result<LookupScratch<Vec<(PathBuf, ProjectPathEntry)>>, ResolverError> {
        let mut budget = self.store_budget(cancel).expect("admitted path lookup");
        let overflow =
            || ResolverError::InvalidRequest("candidate path construction overflow".into());
        let root = budget
            .admit_owned(
                1,
                directory.as_os_str().len(),
                directory.as_os_str().len(),
                || directory.to_path_buf(),
            )
            .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
        let overlays = self
            .store
            .overlay_candidates_admitted(std::slice::from_ref(&*root), names, &mut budget, cancel)
            .map_err(root_store_error)?;
        if !overlays.is_admitted_by(&budget) {
            return Err(ResolverError::InvalidRequest(
                "foreign-root overlay candidates".into(),
            ));
        }
        let count = listing
            .files
            .len()
            .checked_add(overlays.len())
            .ok_or_else(overflow)?;
        let mut entry_bound = 0;
        for source in std::iter::once(entry)
            .chain(self.context.main_source_entry.iter())
            .chain(self.context.explicit_unit_entries.values().flatten())
        {
            let mut bytes = 0usize;
            source
                .visit_recovery_payload(&mut |n| {
                    bytes = bytes
                        .checked_add(n)
                        .ok_or_else(|| "candidate authorization bound overflow".to_string())?;
                    Ok(())
                })
                .map_err(ResolverError::InvalidRequest)?;
            entry_bound = entry_bound.max(bytes);
        }
        let backing = count
            .checked_mul(std::mem::size_of::<(PathBuf, ProjectPathEntry)>())
            .ok_or_else(overflow)?;
        let mut payload = backing;
        let mut key_bytes = 0usize;
        for path in listing.files.iter().chain(overlays.iter()) {
            let path_bytes = path.as_os_str().len();
            let copies = path_bytes
                .checked_mul(8)
                .and_then(|n| n.checked_add(entry_bound.checked_mul(2)?))
                .and_then(|n| n.checked_add(32))
                .ok_or_else(overflow)?;
            payload = payload.checked_add(copies).ok_or_else(overflow)?;
            key_bytes = key_bytes
                .checked_add(
                    path_bytes
                        .checked_mul(8)
                        .and_then(|n| n.checked_add(32))
                        .ok_or_else(overflow)?,
                )
                .ok_or_else(overflow)?;
        }
        // Comparisons, lossy name and sorting keys have a separate temporary
        // construction owner; they never travel with the returned path Vec.
        let work = count
            .checked_mul(
                count
                    .checked_add(self.context.explicit_unit_entries.len())
                    .and_then(|n| n.checked_add(names.len()))
                    .and_then(|n| n.checked_add(2))
                    .ok_or_else(overflow)?,
            )
            .ok_or_else(overflow)?;
        let _sorting = budget
            .admit_owned(
                work,
                key_bytes.checked_mul(count.max(1)).ok_or_else(overflow)?,
                key_bytes.checked_add(backing).ok_or_else(overflow)?,
                || (),
            )
            .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
        let paths = budget.admit_fallible(
            count,
            payload,
            payload,
            crate::compile_time::resolver_error,
            || {
                let mut paths: Vec<(PathBuf, ProjectPathEntry)> = Vec::with_capacity(count);
                for path in &listing.files {
                    if cancel.is_cancelled() {
                        return Err(ResolverError::Cancelled);
                    }
                    if path.file_name().is_some_and(|name| {
                        names
                            .iter()
                            .any(|wanted| name.to_string_lossy().eq_ignore_ascii_case(wanted))
                    }) && !paths
                        .iter()
                        .any(|(previous, _)| path_equivalent(previous, path))
                    {
                        paths.push((
                            path.clone(),
                            self.candidate_entry_for_path(path, entry, None),
                        ));
                    }
                }
                for path in overlays.iter() {
                    if cancel.is_cancelled() {
                        return Err(ResolverError::Cancelled);
                    }
                    if path
                        .parent()
                        .is_none_or(|parent| !path_equivalent(parent, directory))
                    {
                        continue;
                    }
                    if !paths
                        .iter()
                        .any(|(previous, _)| path_equivalent(previous, path))
                    {
                        paths.push((
                            path.clone(),
                            self.candidate_entry_for_path(path, entry, None),
                        ));
                    }
                }
                paths.sort_by_key(|left| path_key(&left.0));
                if cancel.is_cancelled() {
                    return Err(ResolverError::Cancelled);
                }
                Ok(paths)
            },
        )?;
        Ok(LookupScratch::Admitted(paths))
    }

    fn list_directory(
        &mut self,
        directory: &Path,
        entry: &ProjectPathEntry,
        _legacy_route: Option<&LegacyRoute>,
        cancel: &dyn CancellationToken,
        missing_is_incomplete: bool,
    ) -> Result<DirectoryAccess, ResolverError> {
        self.check_cancel(cancel)?;
        if self.compile_time_store.is_some() {
            return self.list_directory_owned(directory, entry, cancel, missing_is_incomplete);
        }
        let key = canonical_path(directory);
        let cache_key = DirectoryCacheKey {
            path: path_identity::path_lookup_key(&key),
            entry: entry.clone(),
            read_policy: self.context.read_policy.clone(),
            missing_is_incomplete,
        };
        if self.compile_time_store.is_none()
            && let Some(listing) = self.directories.get(&cache_key).cloned()
        {
            return Ok(DirectoryAccess::Legacy(listing));
        }
        let request = DirectoryRequest {
            directory: &key,
            entry,
            read_policy: &self.context.read_policy,
        };
        let result = self.store.list_directory(request, cancel);
        let listing = match result {
            Ok(listing) => listing,
            Err(SourceStoreError::NotFound { path: _ }) => DirectoryListing {
                files: Vec::new(),
                directories: Vec::new(),
                stamp: None,
                complete: !missing_is_incomplete,
            },
            Err(SourceStoreError::Cancelled) => return Err(ResolverError::Cancelled),
            Err(error) => return Err(root_store_error(error)),
        };
        self.scanned_entries = self.scanned_entries.saturating_add(
            listing
                .files
                .len()
                .saturating_add(listing.directories.len()),
        );
        self.scanned_bytes = self.scanned_bytes.saturating_add(
            listing
                .files
                .iter()
                .filter_map(|path| fs::symlink_metadata(path).ok())
                .filter(|metadata| !metadata.file_type().is_symlink())
                .map(|metadata| metadata.len() as usize)
                .sum::<usize>(),
        );
        let complete = listing.complete
            && self.scanned_entries <= self.limits.max_directory_entries
            && self.scanned_bytes <= self.limits.max_scanned_bytes;
        let listing = DirectoryListing {
            complete,
            ..listing
        };
        self.record_observation_build(report_path_payload(&key, Some(entry)), cancel, || {
            ResolutionObservation::Directory {
                path: key.clone(),
                entry: entry.clone(),
                stamp: listing.stamp.clone(),
                complete,
            }
        })?;
        if !complete {
            self.diagnostic_args(
                cancel,
                64 + 4 * key.as_os_str().len(),
                format_args!(
                    "directory scan limit or traversal failure under {}",
                    key.display()
                ),
            )?;
        }
        if self.compile_time_store.is_none() {
            self.directories.insert(cache_key, listing.clone());
        }
        Ok(DirectoryAccess::Legacy(listing))
    }

    fn list_directory_owned(
        &mut self,
        directory: &Path,
        entry: &ProjectPathEntry,
        cancel: &dyn CancellationToken,
        missing_is_incomplete: bool,
    ) -> Result<DirectoryAccess, ResolverError> {
        let mut budget = self.store_budget(cancel).expect("typed directory call");
        // All resolver callers canonicalize their directory before this private
        // boundary. Do not let an accidental relative input allocate a cwd of
        // unknown size inside a supposedly bounded key constructor.
        if !directory.is_absolute() && !directory.has_root() {
            return Err(ResolverError::InvalidRequest(
                "typed directory requires a rooted lookup path".into(),
            ));
        }
        // Bound both lexical buffers and component scratch, including minimum
        // small-string allocation capacity for short rooted paths.
        let canonical_bytes = directory
            .as_os_str()
            .len()
            .checked_mul(4)
            .map(|n| n.max(32))
            .ok_or_else(|| {
                ResolverError::InvalidRequest("directory canonicalization overflow".into())
            })?;
        let canonical = budget
            .admit_owned(1, canonical_bytes, canonical_bytes, || {
                canonical_path(directory)
            })
            .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
        let mut key_bytes = canonical
            .as_os_str()
            .len()
            .checked_mul(4)
            .map(|n| n.max(32))
            .ok_or_else(|| ResolverError::InvalidRequest("directory identity overflow".into()))?;
        let mut visit = |n| {
            key_bytes = key_bytes
                .checked_add(n)
                .ok_or_else(|| "directory key overflow".to_string())?;
            Ok(())
        };
        entry
            .visit_recovery_payload(&mut visit)
            .map_err(ResolverError::InvalidRequest)?;
        self.context
            .read_policy
            .visit_owned_clone_payload(&mut visit)
            .map_err(ResolverError::InvalidRequest)?;
        let key = OwnedDirectoryKey(
            budget
                .admit_owned(1, key_bytes, key_bytes, || DirectoryCacheKey {
                    path: path_identity::path_lookup_key(&canonical),
                    entry: entry.clone(),
                    read_policy: self.context.read_policy.clone(),
                    missing_is_incomplete,
                })
                .map_err(|r| crate::compile_time::resolver_error(r, &budget))?,
        );
        // Typed lookups deliberately reread; do not replace freshness checks by
        // a same-root map hit. Neither this request nor callbacks hold the lock.
        let result = self.store.list_directory_admitted(
            DirectoryRequest {
                directory: &canonical,
                entry: &key.0.entry,
                read_policy: &key.0.read_policy,
            },
            &mut budget,
            cancel,
        );
        let (mut listing, cacheable) = match result {
            Ok(listing) => (listing, true),
            Err(SourceStoreError::NotFound { .. }) => (
                budget
                    .admit_owned(1, 0, 0, || DirectoryListing {
                        files: Vec::new(),
                        directories: Vec::new(),
                        stamp: None,
                        complete: !missing_is_incomplete,
                    })
                    .map_err(|r| crate::compile_time::resolver_error(r, &budget))?,
                false,
            ),
            Err(error) => return Err(root_store_error(error)),
        };
        if !listing.is_admitted_by(&budget) {
            return Err(ResolverError::InvalidRequest(
                "foreign-root directory listing".into(),
            ));
        }
        self.scanned_entries = self.scanned_entries.saturating_add(
            listing
                .files
                .len()
                .saturating_add(listing.directories.len()),
        );
        self.scanned_bytes = self.scanned_bytes.saturating_add(
            listing
                .files
                .iter()
                .filter_map(|path| fs::symlink_metadata(path).ok())
                .filter(|metadata| !metadata.file_type().is_symlink())
                .map(|metadata| metadata.len() as usize)
                .sum::<usize>(),
        );
        let complete = listing.complete
            && self.scanned_entries <= self.limits.max_directory_entries
            && self.scanned_bytes <= self.limits.max_scanned_bytes;
        listing.value_mut().complete = complete;
        self.record_observation_build(
            report_path_payload(&canonical, Some(entry)),
            cancel,
            || ResolutionObservation::Directory {
                path: (*canonical).clone(),
                entry: entry.clone(),
                stamp: listing.stamp.clone(),
                complete,
            },
        )?;
        if !complete {
            self.diagnostic_args(
                cancel,
                64 + 4 * canonical.as_os_str().len(),
                format_args!(
                    "directory scan limit or traversal failure under {}",
                    canonical.display()
                ),
            )?;
        }
        let caller = listing
            .into_shared(&mut budget)
            .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
        if cacheable {
            let cached = caller
                .try_clone(&mut budget)
                .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
            let root = self
                .compile_time_store
                .as_ref()
                .expect("typed directory call");
            let mut cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
            cache
                .value_mut()
                .insert_directory(key, cached, &mut budget)?;
        }
        Ok(DirectoryAccess::Admitted(caller))
    }

    fn load_candidate(
        &mut self,
        path: &Path,
        entry: &ProjectPathEntry,
        legacy_route: Option<&LegacyRoute>,
        kind: SourceKind,
        cancel: &dyn CancellationToken,
    ) -> Result<LoadedSource, SourceStoreError> {
        self.load_candidate_with_limit(
            path,
            entry,
            legacy_route,
            kind,
            self.limits.max_source_bytes,
            cancel,
        )
    }

    fn load_candidate_with_limit(
        &mut self,
        path: &Path,
        entry: &ProjectPathEntry,
        legacy_route: Option<&LegacyRoute>,
        kind: SourceKind,
        max_bytes: usize,
        cancel: &dyn CancellationToken,
    ) -> Result<LoadedSource, SourceStoreError> {
        let PreparedSourceLoad { loaded, cached } =
            self.prepare_candidate_with_limit(path, entry, legacy_route, kind, max_bytes, cancel)?;
        // Standalone leaf retains its reviewed report-before-cache ordering.
        if let Some((key, cached)) = cached {
            let mut budget = self.store_budget(cancel).expect("prepared typed cache");
            let root = self
                .compile_time_store
                .as_ref()
                .expect("store call")
                .clone();
            root.view
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert_loaded(key, cached, &mut budget)?;
        } else {
            self.loaded.insert(loaded.id.clone(), loaded.clone());
        }
        Ok(loaded)
    }

    fn prepare_candidate_with_limit(
        &mut self,
        path: &Path,
        entry: &ProjectPathEntry,
        legacy_route: Option<&LegacyRoute>,
        kind: SourceKind,
        max_bytes: usize,
        cancel: &dyn CancellationToken,
    ) -> Result<PreparedSourceLoad, SourceStoreError> {
        self.check_cancel(cancel)
            .map_err(|_| SourceStoreError::Cancelled)?;
        let (path, entry) = self.source_request_scratch(path, entry, cancel)?;
        let stamp = if self.context.read_policy.allows_location(&entry) {
            path_stamp_result(&path).ok().flatten()
        } else {
            None
        };
        self.record_candidate(&path, Some(&entry), stamp.is_some(), cancel)
            .map_err(resolver_report_store_error)?;
        let request = SourceRequest {
            path: &path,
            entry: &entry,
            read_policy: &self.context.read_policy,
            legacy_route,
            kind,
            max_bytes,
        };
        let mut loaded = if let Some(mut budget) = self.store_budget(cancel) {
            self.store.load_admitted(request, &mut budget, cancel)?
        } else {
            self.store.load(request, cancel)?
        };
        if kind == SourceKind::Unit
            && loaded.decoded_text.is_none()
            && std::str::from_utf8(&loaded.bytes).is_err()
            && let Some(mut budget) = self.store_budget(cancel)
        {
            let payload =
                loaded.bytes.len().saturating_mul(3) + loaded_source_metadata_retained(&loaded);
            loaded =
                LoadedSource::admit_construction(&mut budget, loaded.bytes.len(), payload, || {
                    let mut decoded = loaded.clone();
                    decoded.decoded_text = Some(source_text(&loaded).into());
                    Ok(decoded)
                })?;
        }
        if stamp.is_none() {
            self.record_candidate(&path, Some(&entry), true, cancel)
                .map_err(resolver_report_store_error)?;
        }
        let prepared_cache = if let Some(mut budget) = self.store_budget(cancel) {
            let cached = loaded.admitted_copy(&mut budget)?;
            let key = loaded_cache_key(&loaded.id, &mut budget)?;
            Some((key, cached))
        } else {
            None
        };
        let mut sizing_budget = self.store_budget(cancel);
        if let Some(budget) = sizing_budget.as_mut() {
            budget
                .charge(2, 0, 0)
                .map_err(|r| store_admission_error(r, budget))?;
        }
        let mut observation_bytes = loaded
            .id
            .as_str()
            .len()
            .checked_add(loaded.path.as_os_str().len())
            .ok_or(SourceStoreError::AdmissionUnavailable)?;
        if let SourceRevision::Disk {
            read_policy,
            path_entry,
            ..
        } = &loaded.revision
        {
            let mut failure = None;
            let mut sizing_visits = 0usize;
            read_policy
                .visit_owned_clone_payload_with_preflight(&mut |work, bytes| {
                    if let Some(bytes) = bytes {
                        // Each visit reads only O(1) lengths. Poll at bounded
                        // eight-visit intervals, in addition to every admitted
                        // group preflight, rather than one poll per scalar size.
                        if sizing_visits % 8 == 0 && cancel.is_cancelled() {
                            failure = Some(SourceStoreError::Cancelled);
                            return Err("revision sizing cancelled".into());
                        }
                        sizing_visits = (sizing_visits + 1) % 8;
                        observation_bytes = observation_bytes
                            .checked_add(bytes)
                            .ok_or_else(|| "revision sizing overflow".to_string())?;
                    } else if let Some(budget) = sizing_budget.as_mut() {
                        if let Err(reason) = budget.charge(work, 0, 0) {
                            failure = Some(store_admission_error(reason, budget));
                            return Err("revision sizing refused".into());
                        }
                    }
                    Ok(())
                })
                .map_err(|_| failure.unwrap_or(SourceStoreError::AdmissionUnavailable))?;
            if let Some(budget) = sizing_budget.as_mut() {
                budget
                    .charge(2, 0, 0)
                    .map_err(|r| store_admission_error(r, budget))?;
            }
            if cancel.is_cancelled() {
                return Err(SourceStoreError::Cancelled);
            }
            observation_bytes = observation_bytes
                .checked_add(path_entry.path.as_os_str().len())
                .and_then(|n| {
                    n.checked_add(match &path_entry.provenance {
                        ProjectPathProvenance::Mapped { root } => root.as_os_str().len(),
                        _ => 0,
                    })
                })
                .ok_or(SourceStoreError::AdmissionUnavailable)?;
        }
        self.record_observation_build(observation_bytes, cancel, || {
            ResolutionObservation::Payload {
                source_id: loaded.id.clone(),
                path: loaded.path.clone(),
                revision: loaded.revision.clone(),
            }
        })
        .map_err(resolver_report_store_error)?;
        // Truthful read evidence persists if the caller's later cache/seen
        // preparation refuses. No precise cache state is published here.
        Ok(PreparedSourceLoad {
            loaded,
            cached: prepared_cache,
        })
    }

    fn source_request_scratch(
        &self,
        path: &Path,
        entry: &ProjectPathEntry,
        cancel: &dyn CancellationToken,
    ) -> Result<(LookupScratch<PathBuf>, LookupScratch<ProjectPathEntry>), SourceStoreError> {
        if let Some(mut budget) = self.store_budget(cancel) {
            let path = source_request_path(path, &mut budget, cancel)?;
            let local = source_request_entry(&path, entry, &mut budget, cancel)?;
            Ok((
                LookupScratch::Admitted(path),
                LookupScratch::Admitted(local),
            ))
        } else {
            let path = canonical_path(path);
            let local = ProjectPathEntry {
                path: path.clone(),
                provenance: entry.provenance.clone(),
            };
            Ok((LookupScratch::Legacy(path), LookupScratch::Legacy(local)))
        }
    }

    fn record_candidate(
        &mut self,
        path: &Path,
        entry: Option<&ProjectPathEntry>,
        present: bool,
        cancel: &dyn CancellationToken,
    ) -> Result<(), ResolverError> {
        let stamp = entry
            .as_ref()
            .filter(|entry| self.context.read_policy.allows_location(entry))
            .and_then(|_| path_stamp_result(path).ok().flatten());
        self.record_observation_build(report_path_payload(path, entry), cancel, || {
            ResolutionObservation::Candidate {
                path: path.to_path_buf(),
                entry: entry.cloned(),
                stamp,
                present,
            }
        })
    }

    fn build_owned_provider(
        &self,
        mut unit: ResolvedUnit,
        names: OwnedUnitNames,
        requested: &str,
        budget: &mut CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<AdmittedOwned<crate::compile_time::ProviderSource>, ResolverError> {
        if !unit.requested_name.is_empty() || !unit.declared_name.is_empty() {
            return Err(ResolverError::InvalidRequest(
                "provider requires allocation-free name markers".into(),
            ));
        }
        if !names.requested.is_admitted_by(budget) || !names.declared.is_admitted_by(budget) {
            return Err(ResolverError::InvalidRequest(
                "selected name owners belong to another root".into(),
            ));
        }
        let overflow =
            || ResolverError::InvalidRequest("provider construction bound overflow".into());
        let scan_count = self
            .context
            .defines
            .len()
            .checked_add(self.context.conditional_context.defines.len())
            .and_then(|n| n.checked_add(self.context.conditional_context.options.len()))
            .and_then(|n| n.checked_add(self.context.conditional_context.constants.len()))
            .and_then(|n| n.checked_add(self.context.library_conditional_context.defines.len()))
            .and_then(|n| n.checked_add(self.context.library_conditional_context.options.len()))
            .and_then(|n| n.checked_add(self.context.library_conditional_context.constants.len()))
            .ok_or_else(overflow)?;
        let sizing_work = scan_count
            .checked_mul(2)
            .and_then(|n| n.checked_add(2))
            .and_then(|n| n.checked_add(self.context.unit_aliases.len()))
            .ok_or_else(overflow)?;
        budget
            .charge(sizing_work, 0, 0)
            .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
        let reserve = conditional_context_reserve_polled(&self.context, cancel)?;
        if unit.source.decoded_text.is_none() && std::str::from_utf8(&unit.source.bytes).is_err() {
            unit.source = LoadedSource::admit_construction(
                budget,
                unit.source.bytes.len(),
                unit.source.bytes.len().saturating_mul(3)
                    + loaded_source_metadata_retained(&unit.source),
                || {
                    let mut decoded = unit.source.clone();
                    decoded.decoded_text = Some(source_text(&unit.source).into());
                    Ok(decoded)
                },
            )
            .map_err(root_store_error)?;
        }
        let mut alias = requested;
        for (key, target) in &self.context.unit_aliases {
            if cancel.is_cancelled() {
                return Err(ResolverError::Cancelled);
            }
            budget
                .charge(1, key.len().min(requested.len()), 0)
                .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
            if key.eq_ignore_ascii_case(requested) {
                alias = target;
                break;
            }
        }
        let qualifiers = requested
            .len()
            .checked_add(alias.len())
            .and_then(|n| n.checked_add(names.declared.len()))
            .and_then(|n| n.checked_mul(4))
            .and_then(|n| n.checked_add(8 * std::mem::size_of::<String>()))
            .ok_or_else(overflow)?;
        let construction_bytes = reserve.checked_add(qualifiers).ok_or_else(overflow)?;
        let payload = construction_bytes
            .checked_add(loaded_source_metadata_retained(&unit.source))
            .ok_or_else(overflow)?;
        let mut provider = budget
            .admit_owned(scan_count + 1, construction_bytes, payload, || {
                let context = self.context.conditional_context_for(&unit.source.path);
                let qualifiers = qualifiers_for_found(requested, alias, &names.declared);
                crate::compile_time::ProviderSource::new(unit, context, qualifiers)
            })
            .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
        provider
            .move_field(names.requested, budget, |provider, name| {
                provider.unit.requested_name = name
            })
            .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
        provider
            .move_field(names.declared, budget, |provider, name| {
                provider.unit.declared_name = name
            })
            .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
        Ok(provider)
    }

    fn unit_reason_build(
        &mut self,
        bound: usize,
        cancel: &dyn CancellationToken,
        args: std::fmt::Arguments<'_>,
    ) -> Result<String, ResolverError> {
        if !self.owning_unit_reason {
            return Ok(args.to_string());
        }
        if self.callback_unit_reason.is_some() {
            return Err(ResolverError::InvalidRequest(
                "duplicate unit reason construction".into(),
            ));
        }
        let mut budget = self.store_budget(cancel).ok_or_else(|| {
            ResolverError::InvalidRequest("unit reason requires admitted callback root".into())
        })?;
        let owner = build_unit_reason(&mut budget, bound, cancel, args)?;
        self.callback_unit_reason = Some(owner);
        Ok(String::new())
    }

    fn include_reason_build(
        &mut self,
        bound: usize,
        cancel: &dyn CancellationToken,
        args: std::fmt::Arguments<'_>,
    ) -> Result<String, ResolverError> {
        if !self.owning_include_callback {
            return Ok(args.to_string());
        }
        if self.callback_include_reason.is_some() {
            return Err(ResolverError::InvalidRequest(
                "duplicate include reason construction".into(),
            ));
        }
        let mut budget = self.store_budget(cancel).ok_or_else(|| {
            ResolverError::InvalidRequest("include reason requires admitted callback root".into())
        })?;
        let owner = budget
            .admit_owned(1, bound, bound, || {
                use std::fmt::Write;
                let mut reason = String::with_capacity(bound);
                reason.write_fmt(args).expect("include reason formatting");
                reason
            })
            .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
        self.callback_include_reason = Some(owner);
        // Private compatibility plumbing sees only this allocation-free marker.
        // The actual String remains owned until finish constructs the safe
        // callback carrier; no public owning callback returns this marker.
        Ok(String::new())
    }

    fn resolve_include_inner(
        &mut self,
        request: IncludeResolveRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<Resolution<ResolvedSource>, ResolverError> {
        self.check_cancel(cancel)?;
        if self.owning_include_callback {
            let count = self
                .context
                .include_paths
                .len()
                .checked_add(self.context.search_paths.len())
                .ok_or_else(|| {
                    ResolverError::InvalidRequest("include path sizing work overflow".into())
                })?;
            let mut budget = self.store_budget(cancel).expect("owning include root");
            budget
                .charge(count, 0, 0)
                .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
        }
        let paths = self
            .context
            .include_paths
            .iter()
            .chain(&self.context.search_paths)
            .try_fold(0usize, |sum, path| {
                if cancel.is_cancelled() {
                    return Err(ResolverError::Cancelled);
                }
                sum.checked_add(path.as_os_str().len())
                    .and_then(|n| n.checked_add(std::mem::size_of::<ProjectPathEntry>()))
                    .ok_or_else(|| {
                        ResolverError::InvalidRequest("include path sizing overflow".into())
                    })
            })?;
        let reserve = paths
            .checked_mul(4)
            .and_then(|n| n.checked_add(request.including_path.as_os_str().len().checked_mul(8)?))
            .and_then(|n| n.checked_add(request.requested_name.len().checked_mul(8)?))
            .ok_or_else(|| {
                ResolverError::InvalidRequest("include lookup reserve overflow".into())
            })?;
        self.admit_lookup_storage(1, reserve, reserve, cancel)?;
        if request.requested_name.trim().is_empty() {
            return Ok(Resolution::Incomplete {
                reason: self.include_reason_build(
                    34,
                    cancel,
                    format_args!("include directive has no file name"),
                )?,
                candidates: Vec::new(),
            });
        }
        let mut entries = self.context.include_search_entries(request.including_path);
        if let Some(owner_directory) = request.including_path.parent()
            && let Some(owner_entry) =
                self.legacy_entry_for_directory(owner_directory, request.legacy_route)
            && !entries
                .iter()
                .any(|entry| path_equivalent(&entry.path, &owner_entry.path))
        {
            entries.insert(0, owner_entry);
        }
        let mut saw_incomplete = false;
        for entry in entries {
            self.check_cancel(cancel)?;
            self.record_include_search_directory(&entry, cancel)?;
            let resolved = match self
                .context
                .overrides
                .resolve_path(request.requested_name, &entry.path)
            {
                Ok(resolved) => resolved,
                Err(error) => {
                    self.warning_args(
                        cancel,
                        64 + request.requested_name.len() + error.len(),
                        format_args!(
                            "include {} could not be mapped: {error}",
                            request.requested_name
                        ),
                    )?;
                    saw_incomplete = true;
                    continue;
                }
            };
            let path = canonical_path(&resolved.path);
            let mapped_provenance =
                resolved
                    .mapping
                    .as_ref()
                    .map(|mapping| ProjectPathProvenance::Mapped {
                        root: canonical_path(&mapping.to),
                    });
            let candidate_entry =
                self.candidate_entry_for_path(&path, &entry, mapped_provenance.as_ref());
            let case_base = resolved
                .mapping
                .as_ref()
                .map(|mapping| canonical_path(&mapping.to))
                .unwrap_or_else(|| canonical_path(&entry.path));
            let case_lookup_base = self.authorized_case_lookup_base(
                &case_base,
                &path,
                &candidate_entry,
                request.legacy_route,
            );
            match self.load_include_candidate(&path, &candidate_entry, request.legacy_route, cancel)
            {
                // First authorized success always wins. Transfer this inline
                // source directly; no temporary heap collection or invoice.
                Ok(source) => return Ok(Resolution::Found(source)),
                Err(error) if matches!(error.error_ref(), SourceStoreError::NotFound { .. }) => {
                    let Some((case_base, case_base_entry)) = case_lookup_base else {
                        continue;
                    };
                    match self.resolve_case_insensitive_path(
                        &case_base,
                        &path,
                        &case_base_entry,
                        request.legacy_route,
                        cancel,
                    )? {
                        CaseInsensitiveLookup::Found(actual) => {
                            let actual_entry =
                                self.candidate_entry_for_path(&actual, &candidate_entry, None);
                            match self.load_include_candidate(
                                &actual,
                                &actual_entry,
                                request.legacy_route,
                                cancel,
                            ) {
                                Ok(source) => return Ok(Resolution::Found(source)),
                                Err(error)
                                    if matches!(
                                        error.error_ref(),
                                        SourceStoreError::NotFound { .. }
                                    ) => {}
                                Err(error) if error.refusal().is_some() => {
                                    return Err(error.refusal().expect("borrowed refusal"));
                                }
                                Err(error) => {
                                    saw_incomplete = true;
                                    self.include_error_warning(error, cancel)?;
                                    break;
                                }
                            }
                        }
                        CaseInsensitiveLookup::Missing => {}
                        CaseInsensitiveLookup::Ambiguous(paths) => {
                            saw_incomplete = true;
                            if self.owning_include_callback {
                                let mut budget =
                                    self.store_budget(cancel).expect("owning include root");
                                budget
                                    .charge(paths.len(), 0, 0)
                                    .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
                            }
                            let base =
                                request
                                    .requested_name
                                    .len()
                                    .checked_add(128)
                                    .ok_or_else(|| {
                                        ResolverError::InvalidRequest(
                                            "include ambiguity warning bound overflow".into(),
                                        )
                                    })?;
                            let bound = paths.iter().try_fold(base, |sum, path| {
                                if cancel.is_cancelled() {
                                    return Err(ResolverError::Cancelled);
                                }
                                path.as_os_str()
                                    .len()
                                    .checked_mul(4)
                                    .and_then(|n| n.checked_add(2))
                                    .and_then(|n| sum.checked_add(n))
                                    .ok_or_else(|| {
                                        ResolverError::InvalidRequest(
                                            "include ambiguity warning sizing overflow".into(),
                                        )
                                    })
                            })?;
                            self.warning_args(
                                cancel,
                                bound,
                                format_args!(
                                    "include {} has ambiguous case-insensitive path components: {}",
                                    request.requested_name,
                                    ReportPaths(&paths)
                                ),
                            )?;
                            break;
                        }
                        CaseInsensitiveLookup::Incomplete => {
                            saw_incomplete = true;
                            break;
                        }
                    }
                }
                Err(error) if error.refusal().is_some() => {
                    return Err(error.refusal().expect("borrowed refusal"));
                }
                Err(error) => {
                    saw_incomplete = true;
                    self.include_error_warning(error, cancel)?;
                    break;
                }
            }
        }
        if saw_incomplete {
            self.diagnostic_args(
                cancel,
                request
                    .requested_name
                    .len()
                    .checked_add(64)
                    .ok_or_else(|| {
                        ResolverError::InvalidRequest(
                            "incomplete include diagnostic bound overflow".into(),
                        )
                    })?,
                format_args!(
                    "include {} could not be resolved completely",
                    request.requested_name
                ),
            )?;
            Ok(Resolution::Incomplete {
                reason: self.include_reason_build(
                    request
                        .requested_name
                        .len()
                        .checked_add(32)
                        .ok_or_else(|| {
                            ResolverError::InvalidRequest("include reason bound overflow".into())
                        })?,
                    cancel,
                    format_args!("include {} lookup was incomplete", request.requested_name),
                )?,
                candidates: Vec::new(),
            })
        } else {
            Ok(Resolution::Unavailable {
                reason: self.include_reason_build(
                    request
                        .requested_name
                        .len()
                        .checked_add(24)
                        .ok_or_else(|| {
                            ResolverError::InvalidRequest("include reason bound overflow".into())
                        })?,
                    cancel,
                    format_args!("include {} was not found", request.requested_name),
                )?,
            })
        }
    }

    fn record_include_search_directory(
        &mut self,
        entry: &ProjectPathEntry,
        cancel: &dyn CancellationToken,
    ) -> Result<(), ResolverError> {
        self.record_observation_build(
            report_path_payload(&entry.path, Some(entry)),
            cancel,
            || ResolutionObservation::Directory {
                // Keep the configured spelling instead of canonicalizing through a
                // symlink.  Revalidation must detect changes to the search-path
                // link itself as well as changes to its target.
                path: entry.path.clone(),
                entry: entry.clone(),
                stamp: path_stamp_result(&entry.path).ok().flatten(),
                complete: true,
            },
        )
    }

    fn load_include_candidate(
        &mut self,
        path: &Path,
        entry: &ProjectPathEntry,
        legacy_route: Option<&LegacyRoute>,
        cancel: &dyn CancellationToken,
    ) -> Result<LoadedSource, IncludeLoadError> {
        if let Some(mut budget) = self.store_budget(cancel) {
            self.load_include_candidate_owned(path, entry, legacy_route, cancel, &mut budget)
        } else {
            self.load_include_candidate_plain(path, entry, legacy_route, cancel)
                .map_err(IncludeLoadError::Plain)
        }
    }

    /// Consume the private carrier while formatting a separately admitted
    /// report copy from borrowed input. Originals drop only after final use.
    fn include_error_warning(
        &mut self,
        error: IncludeLoadError,
        cancel: &dyn CancellationToken,
    ) -> Result<(), ResolverError> {
        if let Some(mut budget) = self.store_budget(cancel) {
            budget
                .charge(2, 0, 0)
                .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
        }
        let overflow =
            || ResolverError::InvalidRequest("include error report sizing overflow".into());
        let payload = match error.error_ref() {
            SourceStoreError::NotFound { path }
            | SourceStoreError::NotRegularFile { path }
            | SourceStoreError::TooLarge { path, .. } => path.as_os_str().len(),
            SourceStoreError::Incomplete { path, reason }
            | SourceStoreError::Unauthorized { path, reason } => path
                .as_os_str()
                .len()
                .checked_add(reason.len())
                .ok_or_else(overflow)?,
            SourceStoreError::Io { path, message } => path
                .as_os_str()
                .len()
                .checked_add(message.len())
                .ok_or_else(overflow)?,
            SourceStoreError::RootLimit { limit, .. } => limit.len(),
            _ => 0,
        };
        let bound = payload
            .checked_mul(8)
            .and_then(|n| n.checked_add(256))
            .ok_or_else(overflow)?;
        self.warning_args(
            cancel,
            bound,
            format_args!("include candidate could not be loaded: {error:?}"),
        )
    }

    fn load_include_candidate_plain(
        &mut self,
        path: &Path,
        entry: &ProjectPathEntry,
        legacy_route: Option<&LegacyRoute>,
        cancel: &dyn CancellationToken,
    ) -> Result<LoadedSource, SourceStoreError> {
        let source_id = source_id_for_path(path);
        let existing = self
            .loaded
            .values()
            .find(|source| {
                path_equivalent(&source.path, path)
                    && self.can_reuse_loaded_source(path, entry, legacy_route)
            })
            .cloned();
        let known_id = existing.as_ref().map_or(&source_id, |source| &source.id);
        let is_new = !self.seen_include(known_id, None)?;
        if is_new && self.include_files >= self.limits.max_include_files {
            return Err(SourceStoreError::Incomplete {
                path: canonical_path(path),
                reason: format!(
                    "include file limit ({}) reached",
                    self.limits.max_include_files
                ),
            });
        }
        let remaining_bytes = self
            .limits
            .max_include_bytes
            .saturating_sub(self.include_bytes);
        if is_new && remaining_bytes == 0 {
            return Err(SourceStoreError::Incomplete {
                path: canonical_path(path),
                reason: format!(
                    "include byte limit ({}) reached",
                    self.limits.max_include_bytes
                ),
            });
        }
        if let Some(source) = existing {
            if is_new {
                if source.bytes.len() > remaining_bytes {
                    return Err(SourceStoreError::Incomplete {
                        path: source.path,
                        reason: format!(
                            "include byte limit ({}) exceeded",
                            self.limits.max_include_bytes
                        ),
                    });
                }
                self.include_seen.insert(source.id.clone());
                self.include_files = self.include_files.saturating_add(1);
                self.include_bytes = self.include_bytes.saturating_add(source.bytes.len());
            }
            return Ok(source);
        }
        let max_bytes = if is_new {
            self.limits.max_source_bytes.min(remaining_bytes)
        } else {
            self.limits.max_source_bytes
        };
        let source = self.load_candidate_with_limit(
            path,
            entry,
            legacy_route,
            SourceKind::Include,
            max_bytes,
            cancel,
        )?;
        if is_new {
            self.include_seen.insert(source.id.clone());
            self.include_files = self.include_files.saturating_add(1);
            self.include_bytes = self.include_bytes.saturating_add(source.bytes.len());
            if self.include_bytes > self.limits.max_include_bytes {
                return Err(SourceStoreError::Incomplete {
                    path: source.path,
                    reason: format!(
                        "include byte limit ({}) exceeded",
                        self.limits.max_include_bytes
                    ),
                });
            }
        }
        Ok(source)
    }

    fn seen_include(
        &self,
        id: &SourceId,
        budget: Option<&mut CompileTimeBudget<'_>>,
    ) -> Result<bool, SourceStoreError> {
        let cancel = budget.as_ref().map(|b| b.cancel_token());
        if let Some(budget) = budget {
            let count = self
                .compile_time_caches
                .len()
                .checked_add(1)
                .ok_or(SourceStoreError::AdmissionUnavailable)?;
            let bytes = count
                .checked_mul(id.as_str().len())
                .ok_or(SourceStoreError::AdmissionUnavailable)?;
            budget
                .charge(count, bytes, 0)
                .map_err(|r| store_admission_error(r, budget))?;
        }
        if self.include_seen.contains(id) {
            return Ok(true);
        }
        for root in &self.compile_time_caches {
            if cancel.is_some_and(|cancel| cancel.is_cancelled()) {
                return Err(SourceStoreError::Cancelled);
            }
            if root
                .view
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .include_seen
                .contains(id)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn load_include_candidate_owned(
        &mut self,
        path: &Path,
        entry: &ProjectPathEntry,
        route: Option<&LegacyRoute>,
        cancel: &dyn CancellationToken,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<LoadedSource, IncludeLoadError> {
        let root = self
            .compile_time_store
            .as_ref()
            .expect("typed include reuse")
            .clone();
        if !root.view.is_admitted_by(budget) {
            return Err(SourceStoreError::AdmissionUnavailable.into());
        }
        let query = include_query(path, budget)?;
        let existing = {
            let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
            budget
                .charge(cache.loaded.capacity(), 0, 0)
                .map_err(|r| store_admission_error(r, budget))?;
            let mut existing = None;
            for source in cache.loaded.values() {
                let canonical = source_request_path(&source.path, budget, cancel)?;
                let identity = include_identity(&canonical, budget)?;
                budget
                    .charge(
                        1,
                        identity
                            .len()
                            .checked_add(query.identity.len())
                            .ok_or(SourceStoreError::AdmissionUnavailable)?,
                        0,
                    )
                    .map_err(|r| store_admission_error(r, budget))?;
                if *identity == *query.identity
                    && self.can_reuse_include_owned(&query, entry, route, budget)?
                {
                    existing = Some(source.admitted_copy(budget)?);
                    break;
                }
            }
            existing
        };
        let known_id = existing.as_ref().map_or(&*query.id, |source| &source.id);
        let is_new = !self.seen_include(known_id, Some(budget))?;
        if is_new && self.include_files >= self.limits.max_include_files {
            return Err(include_limit_error(
                query.canonical,
                "file",
                self.limits.max_include_files,
                "reached",
                budget,
            )?);
        }
        let remaining = self
            .limits
            .max_include_bytes
            .saturating_sub(self.include_bytes);
        if is_new && remaining == 0 {
            return Err(include_limit_error(
                query.canonical,
                "byte",
                self.limits.max_include_bytes,
                "reached",
                budget,
            )?);
        }
        let prepared_source = if let Some(source) = existing {
            if is_new && source.bytes.len() > remaining {
                let path = include_limit_path_copy(&source.path, budget)?;
                return Err(include_limit_error(
                    path,
                    "byte",
                    self.limits.max_include_bytes,
                    "exceeded",
                    budget,
                )?);
            }
            PreparedSourceLoad {
                loaded: source,
                cached: None,
            }
        } else {
            self.prepare_candidate_with_limit(
                path,
                entry,
                route,
                SourceKind::Include,
                if is_new {
                    self.limits.max_source_bytes.min(remaining)
                } else {
                    self.limits.max_source_bytes
                },
                cancel,
            )?
        };
        let PreparedSourceLoad {
            loaded: source,
            cached,
        } = prepared_source;
        let (next_files, next_bytes) = if is_new {
            let files = self
                .include_files
                .checked_add(1)
                .ok_or(SourceStoreError::AdmissionUnavailable)?;
            let bytes = self
                .include_bytes
                .checked_add(source.bytes.len())
                .ok_or(SourceStoreError::AdmissionUnavailable)?;
            if bytes > self.limits.max_include_bytes {
                let path = include_limit_path_copy(&source.path, budget)?;
                return Err(include_limit_error(
                    path,
                    "byte",
                    self.limits.max_include_bytes,
                    "exceeded",
                    budget,
                )?);
            }
            (files, bytes)
        } else {
            (self.include_files, self.include_bytes)
        };
        let seen_key = if is_new {
            budget
                .charge(
                    1,
                    query
                        .id
                        .as_str()
                        .len()
                        .checked_add(source.id.as_str().len())
                        .ok_or(SourceStoreError::AdmissionUnavailable)?,
                    0,
                )
                .map_err(|r| store_admission_error(r, budget))?;
            let key = if *query.id == source.id {
                OwnedLoadedKey(query.id)
            } else {
                loaded_cache_key(&source.id, budget)?
            };
            Some(key)
        } else {
            None
        };
        let mut cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
        let loaded = if let Some((key, value)) = cached {
            Some(cache.prepare_loaded(key, value, budget)?)
        } else {
            None
        };
        let seen = if let Some(key) = seen_key {
            cache.prepare_seen(key, budget)?
        } else {
            None
        };
        // Final check includes both admitted receiving backings at their honest
        // old-plus-new peak. No callback/admission follows either commit.
        budget
            .charge(0, 0, 0)
            .map_err(|r| store_admission_error(r, budget))?;
        if let Some(loaded) = loaded {
            cache.value_mut().commit_loaded(loaded);
        }
        if let Some(seen) = seen {
            cache.value_mut().commit_seen(seen);
        }
        self.include_files = next_files;
        self.include_bytes = next_bytes;
        Ok(source)
    }

    fn can_reuse_include_owned(
        &self,
        query: &IncludeQuery,
        entry: &ProjectPathEntry,
        route: Option<&LegacyRoute>,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<bool, SourceStoreError> {
        let canonical = source_request_path(&entry.path, budget, budget.cancel_token())?;
        let identity = include_identity(&canonical, budget)?;
        budget
            .charge(
                1,
                identity
                    .len()
                    .checked_add(query.identity.len())
                    .ok_or(SourceStoreError::AdmissionUnavailable)?,
                0,
            )
            .map_err(|r| store_admission_error(r, budget))?;
        if *identity != *query.identity {
            return Ok(false);
        }
        let legacy = if let Some(route) = route {
            let source = source_request_path(&route.source_path, budget, budget.cancel_token())?;
            let sibling =
                source_request_path(&route.sibling_directory, budget, budget.cancel_token())?;
            let parent_bytes = source
                .as_os_str()
                .len()
                .checked_add(query.canonical.as_os_str().len())
                .ok_or(SourceStoreError::AdmissionUnavailable)?;
            budget
                .charge(
                    parent_bytes
                        .checked_add(2)
                        .ok_or(SourceStoreError::AdmissionUnavailable)?,
                    parent_bytes,
                    0,
                )
                .map_err(|r| store_admission_error(r, budget))?;
            if let (Some(source_parent), Some(query_parent)) =
                (source.parent(), query.canonical.parent())
            {
                let sibling = include_identity(&sibling, budget)?;
                let parent = include_identity(source_parent, budget)?;
                let query_parent = include_identity(query_parent, budget)?;
                let bytes = sibling
                    .len()
                    .checked_mul(2)
                    .and_then(|n| n.checked_add(parent.len()))
                    .and_then(|n| n.checked_add(query_parent.len()))
                    .ok_or(SourceStoreError::AdmissionUnavailable)?;
                budget
                    .charge(2, bytes, 0)
                    .map_err(|r| store_admission_error(r, budget))?;
                *parent == *sibling && *query_parent == *sibling
            } else {
                false
            }
        } else {
            false
        };
        Ok((legacy
            && matches!(entry.provenance, ProjectPathProvenance::LegacyNative)
            && self.context.read_policy.allows_legacy_route_entry(entry))
            || self.context.read_policy.allows_entry(entry))
    }

    fn can_reuse_loaded_source(
        &self,
        path: &Path,
        entry: &ProjectPathEntry,
        legacy_route: Option<&LegacyRoute>,
    ) -> bool {
        let path = canonical_path(path);
        if !path_equivalent(&path, &entry.path) {
            return false;
        }
        let legacy = legacy_route.is_some_and(|route| legacy_route_authorizes(&path, route))
            && matches!(entry.provenance, ProjectPathProvenance::LegacyNative)
            && self.context.read_policy.allows_legacy_route_entry(entry);
        legacy || self.context.read_policy.allows_entry(entry)
    }

    fn authorized_case_lookup_base(
        &self,
        base: &Path,
        target: &Path,
        fallback_entry: &ProjectPathEntry,
        legacy_route: Option<&LegacyRoute>,
    ) -> Option<(PathBuf, ProjectPathEntry)> {
        let target = canonical_path(target);
        let mut candidate = canonical_path(base);
        loop {
            if path_starts_with(&target, &candidate) {
                let entry = self
                    .context
                    .path_entry_for(&candidate)
                    .or_else(|| self.legacy_entry_for_directory(&candidate, legacy_route))
                    .or_else(|| {
                        path_equivalent(&candidate, base).then(|| ProjectPathEntry {
                            path: candidate.clone(),
                            provenance: fallback_entry.provenance.clone(),
                        })
                    });
                if let Some(entry) = entry {
                    let entry = ProjectPathEntry {
                        path: candidate.clone(),
                        provenance: entry.provenance,
                    };
                    if self.context.read_policy.allows_location(&entry) {
                        return Some((candidate, entry));
                    }
                }
            }
            if !candidate.pop() {
                break;
            }
        }
        None
    }

    fn resolve_case_insensitive_path(
        &mut self,
        base: &Path,
        path: &Path,
        base_entry: &ProjectPathEntry,
        _legacy_route: Option<&LegacyRoute>,
        cancel: &dyn CancellationToken,
    ) -> Result<CaseInsensitiveLookup, ResolverError> {
        if self.compile_time_store.is_some() {
            return self.resolve_case_insensitive_path_owned(
                base,
                path,
                base_entry,
                _legacy_route,
                cancel,
            );
        }
        let base = canonical_path(base);
        let path = canonical_path(path);
        let Some(relative) = relative_components(&base, &path) else {
            return Ok(CaseInsensitiveLookup::Missing);
        };
        let mut current = base;
        for wanted in relative {
            self.check_cancel(cancel)?;
            let current_entry = ProjectPathEntry {
                path: current.clone(),
                provenance: base_entry.provenance.clone(),
            };
            let listing =
                self.list_directory(&current, &current_entry, _legacy_route, cancel, false)?;
            let mut matches = listing
                .files
                .iter()
                .chain(listing.directories.iter())
                .filter(|candidate| {
                    candidate.file_name().is_some_and(|name| {
                        name.to_string_lossy()
                            .eq_ignore_ascii_case(&wanted.to_string_lossy())
                    })
                })
                .cloned()
                .collect::<Vec<_>>();
            matches.sort_by_key(|left| path_key(left));
            if !listing.complete {
                self.diagnostic_args(
                    cancel,
                    64 + 4 * current.as_os_str().len(),
                    format_args!(
                        "case-insensitive lookup under {} was incomplete",
                        current.display()
                    ),
                )?;
                return Ok(CaseInsensitiveLookup::Incomplete);
            }
            if let Some(exact) = matches
                .iter()
                .find(|candidate| candidate.file_name().is_some_and(|name| name == wanted))
            {
                current = exact.clone();
            } else if matches.len() == 1 {
                current = matches.pop().expect("one path match");
            } else if matches.len() > 1 {
                return Ok(CaseInsensitiveLookup::Ambiguous(matches.into()));
            } else {
                return Ok(CaseInsensitiveLookup::Missing);
            }
        }
        Ok(CaseInsensitiveLookup::Found(current.into()))
    }

    fn resolve_case_insensitive_path_owned(
        &mut self,
        base: &Path,
        path: &Path,
        base_entry: &ProjectPathEntry,
        legacy_route: Option<&LegacyRoute>,
        cancel: &dyn CancellationToken,
    ) -> Result<CaseInsensitiveLookup, ResolverError> {
        let mut budget = self.store_budget(cancel).expect("typed case lookup");
        let overflow = || ResolverError::InvalidRequest("case match construction overflow".into());
        if (!base.is_absolute() && !base.has_root()) || (!path.is_absolute() && !path.has_root()) {
            return Err(ResolverError::InvalidRequest(
                "typed case matching requires rooted paths".into(),
            ));
        }
        let canonical_bound = |path: &Path| {
            path.as_os_str()
                .len()
                .checked_mul(4)
                .map(|n| n.max(32))
                .ok_or_else(overflow)
        };
        let base_bytes = canonical_bound(base)?;
        let base = budget
            .admit_owned(1, base_bytes, base_bytes, || canonical_path(base))
            .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
        let path_bytes = canonical_bound(path)?;
        let path = budget
            .admit_owned(1, path_bytes, path_bytes, || canonical_path(path))
            .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
        let Some(relative) = relative_components_owned(&base, &path, &mut budget, cancel)? else {
            return Ok(CaseInsensitiveLookup::Missing);
        };
        drop(path);
        if relative.is_empty() {
            return Ok(CaseInsensitiveLookup::Found(LookupScratch::Admitted(
                copy_case_path(&base, &mut budget)?,
            )));
        }
        let mut current = base;
        for wanted in relative.iter() {
            self.check_cancel(cancel)?;
            let mut provenance_bytes = 0usize;
            if let ProjectPathProvenance::Mapped { root } = &base_entry.provenance {
                provenance_bytes = root.as_os_str().len();
            }
            let entry_bytes = current
                .as_os_str()
                .len()
                .checked_add(provenance_bytes)
                .ok_or_else(overflow)?;
            let current_entry = budget
                .admit_owned(1, entry_bytes, entry_bytes, || ProjectPathEntry {
                    path: (*current).clone(),
                    provenance: base_entry.provenance.clone(),
                })
                .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
            let listing =
                self.list_directory(&current, &current_entry, legacy_route, cancel, false)?;
            let count = listing
                .files
                .len()
                .checked_add(listing.directories.len())
                .ok_or_else(overflow)?;
            let index_bytes = count
                .checked_mul(std::mem::size_of::<usize>())
                .ok_or_else(overflow)?;
            let mut sort_bytes = case_sort_sizing(&listing, wanted, &mut budget, cancel)?;
            sort_bytes = sort_bytes
                .checked_add(index_bytes.checked_mul(2).ok_or_else(overflow)?)
                .ok_or_else(overflow)?;
            let work = count
                .checked_mul(count.checked_add(1).ok_or_else(overflow)?)
                .ok_or_else(overflow)?;
            let _sorting = budget
                .admit_owned(
                    work,
                    sort_bytes.checked_mul(count.max(1)).ok_or_else(overflow)?,
                    sort_bytes,
                    || (),
                )
                .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
            let candidate_at = |index: usize| {
                if index < listing.files.len() {
                    &listing.files[index]
                } else {
                    &listing.directories[index - listing.files.len()]
                }
            };
            let matches = budget.admit_fallible(
                count,
                index_bytes,
                index_bytes,
                crate::compile_time::resolver_error,
                || {
                    let mut matches: Vec<usize> = Vec::with_capacity(count);
                    for (index, candidate) in listing
                        .files
                        .iter()
                        .chain(listing.directories.iter())
                        .enumerate()
                    {
                        if cancel.is_cancelled() {
                            return Err(ResolverError::Cancelled);
                        }
                        if candidate.file_name().is_some_and(|name| {
                            name.to_string_lossy()
                                .eq_ignore_ascii_case(&wanted.to_string_lossy())
                        }) && !matches
                            .iter()
                            .any(|previous| path_equivalent(candidate_at(*previous), candidate))
                        {
                            matches.push(index);
                        }
                    }
                    matches.sort_by_key(|index| path_key(candidate_at(*index)));
                    if cancel.is_cancelled() {
                        return Err(ResolverError::Cancelled);
                    }
                    Ok(matches)
                },
            )?;
            if !listing.complete {
                let bound = current
                    .as_os_str()
                    .len()
                    .checked_mul(4)
                    .and_then(|n| n.checked_add(64))
                    .ok_or_else(overflow)?;
                self.diagnostic_args(
                    cancel,
                    bound,
                    format_args!(
                        "case-insensitive lookup under {} was incomplete",
                        current.display()
                    ),
                )?;
                return Ok(CaseInsensitiveLookup::Incomplete);
            }
            let exact = matches.iter().find(|index| {
                candidate_at(**index)
                    .file_name()
                    .is_some_and(|name| name == wanted)
            });
            if let Some(index) = exact.or_else(|| (matches.len() == 1).then(|| &matches[0])) {
                let selected = candidate_at(*index);
                let next = copy_case_path(selected, &mut budget)?;
                // New selected-path storage is admitted while the old current
                // owner is live. Assignment destroys old storage before refund.
                current = next;
            } else if matches.len() > 1 {
                let paths = copy_case_paths(&listing, &matches, &mut budget, cancel)?;
                return Ok(CaseInsensitiveLookup::Ambiguous(LookupScratch::Admitted(
                    paths,
                )));
            } else {
                return Ok(CaseInsensitiveLookup::Missing);
            }
        }
        Ok(CaseInsensitiveLookup::Found(LookupScratch::Admitted(
            current,
        )))
    }

    fn filename_catalogue_candidates(
        &mut self,
        unit_name: &str,
        cancel: &dyn CancellationToken,
    ) -> Result<Vec<CandidateGroup<'static>>, ResolverError> {
        let initial = self
            .workspace_roots
            .iter()
            .map(|p| p.as_os_str().len() + std::mem::size_of::<PathBuf>())
            .sum::<usize>()
            + self
                .context
                .unit_namespaces
                .iter()
                .map(|n| n.len() + std::mem::size_of::<String>())
                .sum::<usize>()
            + unit_name.len();
        self.admit_lookup_storage(
            1,
            initial.saturating_mul(8),
            initial.saturating_mul(8),
            cancel,
        )?;
        let tiers = filename_tiers(unit_name, &self.context.unit_namespaces);
        let mut paths_by_tier: Vec<Vec<(PathBuf, ProjectPathEntry)>> =
            vec![Vec::new(); tiers.len()];
        // Projectless lookup is deliberately rooted at the caller-supplied
        // workspace roots.  Reusing the context's ordered unit paths here
        // would turn a configured search path into a recursive project-wide
        // catalogue and would change the precedence boundary.
        let roots = self.workspace_roots.clone();
        for root in roots {
            let catalogue = self.catalogue(&root, cancel)?;
            if !catalogue.complete {
                return Err(ResolverError::LimitExceeded {
                    limit: "filename catalogue",
                    observed: self.scanned_entries,
                    maximum: self.limits.max_package_catalogue_entries,
                });
            }
            for (index, names) in tiers.iter().enumerate() {
                for name in names {
                    if let Some(found) = catalogue.entries.get(&name.to_ascii_lowercase()) {
                        let bytes = found
                            .iter()
                            .map(|p| {
                                4 * p.as_os_str().len()
                                    + 4 * std::mem::size_of::<(PathBuf, ProjectPathEntry)>()
                            })
                            .sum();
                        self.admit_lookup_storage(1, bytes, bytes, cancel)?;
                        paths_by_tier[index].extend(found.iter().filter_map(|path| {
                            self.context
                                .path_entry_for(path)
                                .map(|entry| (path.clone(), entry))
                        }));
                    }
                }
            }
            for (index, names) in tiers.iter().enumerate() {
                let owned = if let Some(mut budget) = self.store_budget(cancel) {
                    Some(
                        self.store
                            .overlay_candidates_admitted(
                                std::slice::from_ref(&root),
                                names,
                                &mut budget,
                                cancel,
                            )
                            .map_err(root_store_error)?,
                    )
                } else {
                    None
                };
                let overlays = if let Some(ref owned) = owned {
                    let bytes = owned
                        .iter()
                        .map(|p| {
                            4 * p.as_os_str().len()
                                + 4 * std::mem::size_of::<(PathBuf, ProjectPathEntry)>()
                        })
                        .sum();
                    self.admit_lookup_storage(1, bytes, bytes, cancel)?;
                    owned.to_vec()
                } else {
                    self.store
                        .overlay_candidates(std::slice::from_ref(&root), names)
                };
                for path in overlays {
                    let Some(entry) = self.context.path_entry_for(&path) else {
                        continue;
                    };
                    if !paths_by_tier[index]
                        .iter()
                        .any(|(existing, _)| path_equivalent(existing, &path))
                    {
                        paths_by_tier[index].push((path, entry));
                    }
                }
            }
        }
        Ok(paths_by_tier
            .into_iter()
            .map(|paths| CandidateGroup {
                paths: paths.into(),
                legacy_route: None,
            })
            .collect())
    }

    fn catalogue(
        &mut self,
        root: &Path,
        cancel: &dyn CancellationToken,
    ) -> Result<Arc<Catalogue>, ResolverError> {
        let initial = 4 * root.as_os_str().len() + 4 * std::mem::size_of::<Catalogue>();
        self.admit_lookup_storage(1, initial, initial, cancel)?;
        let root = canonical_path(root);
        // Every spelling of a directory on a case-insensitive volume is one
        // catalogue; distinct directories never share one.
        let root_key = path_identity::path_lookup_key(&root);
        if self.compile_time_store.is_none()
            && let Some(catalogue) = self.package_catalogues.get(&root_key).cloned()
        {
            return Ok(catalogue);
        }
        if self.package_catalogue_count >= self.limits.max_package_catalogues {
            let maximum = self.limits.max_package_catalogues;
            self.diagnostic_args(
                cancel,
                64,
                format_args!("package/source catalogue limit ({}) reached", maximum),
            )?;
            return Ok(Arc::new(Catalogue {
                entries: HashMap::new(),
                complete: false,
            }));
        }
        self.package_catalogue_count += 1;
        if self.compile_time_store.is_none()
            && let Some(session_cache) = &self.session_cache
            && let Some(cached) = session_cache.get(&root_key)
        {
            for observation in cached.observations.iter().cloned() {
                self.record_observation(observation);
            }
            for warning in cached.warnings.iter().cloned() {
                self.warn(warning);
            }
            for reason in cached.incomplete_reasons.iter().cloned() {
                self.mark_incomplete(reason);
            }
            self.package_catalogues
                .insert(root_key, Arc::clone(&cached.catalogue));
            return Ok(cached.catalogue);
        }
        let observations_before = self.report.observations.len();
        let warnings_before = self.report.warnings.len();
        let incomplete_reasons_before = self.report.incomplete_reasons.len();
        let mut queue = VecDeque::from([root.clone()]);
        let mut visited = HashSet::new();
        let mut entries = HashMap::new();
        let mut complete = true;
        let mut catalogue_entries = 0usize;
        while let Some(directory) = queue.pop_front() {
            self.check_cancel(cancel)?;
            if !visited.insert(path_identity::path_lookup_key(&directory)) {
                continue;
            }
            let Some(entry) = self.context.path_entry_for(&directory) else {
                complete = false;
                self.diagnostic_args(
                    cancel,
                    128 + 4 * directory.as_os_str().len(),
                    format_args!(
                        "catalogue directory {} is outside the requester read policy",
                        directory.display()
                    ),
                )?;
                break;
            };
            let listing = self.list_directory(&directory, &entry, None, cancel, true)?;
            let copies = listing
                .files
                .iter()
                .chain(&listing.directories)
                .map(|p| {
                    8 * (p.as_os_str().len()
                        + std::mem::size_of::<PathBuf>()
                        + std::mem::size_of::<String>()
                        + 4 * std::mem::size_of::<usize>())
                })
                .sum::<usize>();
            self.admit_lookup_storage(1, copies, copies, cancel)?;
            complete &= listing.complete;
            catalogue_entries = catalogue_entries.saturating_add(
                listing
                    .directories
                    .len()
                    .saturating_add(listing.files.len()),
            );
            if catalogue_entries > self.limits.max_package_catalogue_entries {
                complete = false;
                break;
            }
            for child in &listing.directories {
                let Some(child_entry) = self.context.path_entry_for(child) else {
                    continue;
                };
                if self.context.read_policy.allows_location(&child_entry) {
                    queue.push_back(child.clone());
                }
            }
            for file in &listing.files {
                let Some(file_entry) = self.context.path_entry_for(file) else {
                    continue;
                };
                if !self.context.read_policy.allows_location(&file_entry) {
                    continue;
                }
                if file.extension().is_some_and(|extension| {
                    extension.eq_ignore_ascii_case("pas")
                        || extension.eq_ignore_ascii_case("dpk")
                        || extension.eq_ignore_ascii_case("dproj")
                }) {
                    let key = file
                        .file_name()
                        .map(|name| name.to_string_lossy().to_ascii_lowercase())
                        .unwrap_or_default();
                    entries
                        .entry(key)
                        .or_insert_with(Vec::new)
                        .push(file.clone());
                }
            }
            if !complete {
                break;
            }
        }
        let catalogue = Arc::new(Catalogue { entries, complete });
        let cached = CachedCatalogue {
            catalogue: Arc::clone(&catalogue),
            observations: self.report.observations[observations_before..]
                .to_vec()
                .into(),
            warnings: self.report.warnings[warnings_before..].to_vec().into(),
            incomplete_reasons: self.report.incomplete_reasons[incomplete_reasons_before..]
                .to_vec()
                .into(),
        };
        self.package_catalogues
            .insert(root_key.clone(), Arc::clone(&catalogue));
        if let Some(session_cache) = &self.session_cache {
            session_cache.insert(root_key, cached, self.limits.max_package_catalogues);
        }
        Ok(catalogue)
    }

    fn package_lookup_for_unit(
        &mut self,
        requested: &str,
        lookup: &str,
        legacy_route: Option<&LegacyRoute>,
        cancel: &dyn CancellationToken,
    ) -> Result<Resolution<ResolvedUnit>, ResolverError> {
        if self.compile_time_store.is_some() {
            // The XML/package metadata reader is not a SourceStore payload
            // capability. Never invoke its legacy transitive reader under a
            // typed root and pretend later catalogue charges admitted that IO.
            let reason = "package metadata has no admitted compile-time reader";
            if !self.owning_unit_reason {
                self.admit_lookup_storage(1, reason.len(), reason.len(), cancel)?;
            }
            return Ok(Resolution::Incomplete {
                reason: self.unit_reason_build(reason.len(), cancel, format_args!("{reason}"))?,
                candidates: vec![],
            });
        }
        if self.package_lookups >= self.limits.max_package_lookups {
            self.mark_incomplete(format!(
                "package lookup limit ({}) reached",
                self.limits.max_package_lookups
            ));
            return Ok(Resolution::Incomplete {
                reason: "package lookup limit reached".to_string(),
                candidates: Vec::new(),
            });
        }
        if self.context.packages.len() > self.limits.max_package_lookups {
            self.mark_incomplete(format!(
                "named package lookup limit ({}) reached",
                self.limits.max_package_lookups
            ));
            return Ok(Resolution::Incomplete {
                reason: "named package lookup limit reached".to_string(),
                candidates: Vec::new(),
            });
        }
        self.package_lookups += 1;
        let mut package_candidates = Vec::new();
        for package in self.context.packages.clone() {
            self.check_cancel(cancel)?;
            let package_key = package_stem(&package);
            let mut dpk_descriptors = Vec::new();
            let mut dproj_descriptors = Vec::new();
            for root in self.package_roots() {
                let catalogue = self.catalogue(&root, cancel)?;
                let dpk_key = format!("{package_key}.dpk");
                let dproj_key = format!("{package_key}.dproj");
                if let Some(paths) = catalogue.entries.get(&dpk_key) {
                    dpk_descriptors.extend(paths.iter().cloned());
                }
                if let Some(paths) = catalogue.entries.get(&dproj_key) {
                    dproj_descriptors.extend(paths.iter().cloned());
                }
                if !catalogue.complete {
                    return Ok(Resolution::Incomplete {
                        reason: format!("package catalogue for {package} is incomplete"),
                        candidates: Vec::new(),
                    });
                }
            }
            let mut descriptors = if dpk_descriptors.is_empty() {
                dproj_descriptors
            } else {
                dpk_descriptors
            };
            descriptors.sort_by_key(|left| path_key(left));
            descriptors.dedup_by(|left, right| path_equivalent(left, right));
            if descriptors.is_empty() {
                self.warn(format!(
                    "source for package {package} was not found under configured workspace/source roots; compiled-only package skipped"
                ));
                continue;
            }
            if descriptors.len() > 1 {
                self.warn(format!(
                    "ambiguous package {package}; matching source descriptors were found"
                ));
                return Ok(Resolution::Ambiguous {
                    candidates: descriptors
                        .into_iter()
                        .map(|path| ResolutionCandidate {
                            path,
                            declared_name: None,
                        })
                        .collect(),
                });
            }
            let descriptor = descriptors.pop().expect("one descriptor");
            self.check_cancel(cancel)?;
            let Some(entry) = self.context.path_entry_for(&descriptor) else {
                self.mark_incomplete(format!(
                    "package descriptor {} is outside the requester read policy",
                    descriptor.display()
                ));
                return Ok(Resolution::Incomplete {
                    reason: "package descriptor is outside the requester read policy".to_string(),
                    candidates: Vec::new(),
                });
            };
            self.record_candidate(&descriptor, Some(&entry), true, cancel)?;
            let package_read = read_package_metadata_with_observations(
                &descriptor,
                &self.context.selected_project_options(),
                &self.context.overrides,
                &self.context.read_policy,
                &entry,
            );
            let read = match package_read {
                Ok(read) => read,
                Err(error) => {
                    self.warn(format!("source package {package} was skipped: {error}"));
                    self.mark_incomplete(format!(
                        "source package {package} metadata is incomplete: {error}"
                    ));
                    return Ok(Resolution::Incomplete {
                        reason: format!("source package {package} metadata is incomplete"),
                        candidates: Vec::new(),
                    });
                }
            };
            self.check_cancel(cancel)?;
            for observation in read.metadata.metadata_observations.iter().cloned() {
                self.record_observation(ResolutionObservation::Metadata(observation));
            }
            for observation in read.observations {
                self.record_observation(ResolutionObservation::ProjectRead(observation));
            }
            for warning in read.metadata.warnings {
                self.warn(warning);
            }
            if read.metadata.incomplete {
                self.mark_incomplete(format!(
                    "source package {package} metadata omitted an unsafe candidate"
                ));
                return Ok(Resolution::Incomplete {
                    reason: format!("source package {package} metadata is incomplete"),
                    candidates: Vec::new(),
                });
            }
            let mut package_candidate_count = 0usize;
            for (unit_name, entries) in read.metadata.unit_entries {
                if !declared_name_matches(Some(&unit_name), requested, lookup, &self.context) {
                    continue;
                }
                for entry in entries {
                    if package_candidate_count >= self.limits.max_package_unit_candidates {
                        self.mark_incomplete(format!(
                            "package unit candidate limit ({}) reached",
                            self.limits.max_package_unit_candidates
                        ));
                        return Ok(Resolution::Incomplete {
                            reason: "package unit candidate limit reached".to_string(),
                            candidates: Vec::new(),
                        });
                    }
                    package_candidate_count = package_candidate_count.saturating_add(1);
                    match self.load_candidate(
                        &entry.path,
                        &entry,
                        legacy_route,
                        SourceKind::Unit,
                        cancel,
                    ) {
                        Ok(source) => {
                            let metadata = self.source_metadata(&source);
                            if declared_name_matches(
                                metadata.declared_name.as_deref(),
                                requested,
                                lookup,
                                &self.context,
                            ) {
                                package_candidates.push((
                                    source,
                                    ResolutionCandidate {
                                        path: entry.path.clone(),
                                        declared_name: metadata.declared_name.clone(),
                                    },
                                ));
                            }
                        }
                        Err(SourceStoreError::NotFound { .. }) => {}
                        Err(SourceStoreError::Cancelled) => return Err(ResolverError::Cancelled),
                        Err(error) => {
                            self.warn(format!(
                                "package unit candidate could not be loaded: {error:?}"
                            ));
                            self.mark_incomplete(format!(
                                "package unit candidate {} could not be loaded",
                                entry.path.display()
                            ));
                            return Ok(Resolution::Incomplete {
                                reason: "package unit candidate could not be loaded".to_string(),
                                candidates: Vec::new(),
                            });
                        }
                    }
                }
            }
        }
        package_candidates.sort_by_key(|left| path_key(&left.0.path));
        package_candidates.dedup_by(|left, right| left.0.id == right.0.id);
        match package_candidates.len() {
            0 => Ok(Resolution::Unavailable {
                reason: format!("no package unit candidate for {requested}"),
            }),
            1 => {
                let (source, candidate) = package_candidates.pop().expect("one package candidate");
                Ok(Resolution::Found(ResolvedUnit {
                    requested_name: requested.to_string(),
                    declared_name: candidate
                        .declared_name
                        .unwrap_or_else(|| lookup.to_string()),
                    source,
                }))
            }
            _ => {
                self.warn(format!(
                    "ambiguous package unit {requested}: multiple valid candidates"
                ));
                Ok(Resolution::Ambiguous {
                    candidates: package_candidates
                        .into_iter()
                        .map(|(_, candidate)| candidate)
                        .collect(),
                })
            }
        }
    }

    fn package_roots(&self) -> Vec<PathBuf> {
        let mut roots = self.workspace_roots.clone();
        for configured_root in self.context.overrides.read_roots() {
            let root = resolved_package_root(&self.context, &configured_root);
            if !root.exists() && !context_uses_package_root(&self.context, &root) {
                continue;
            }
            if !roots
                .iter()
                .any(|existing| path_equivalent(existing, &root))
            {
                roots.push(root);
            }
        }
        roots
    }

    // Project include bindings are kept in a session-local queue so the public
    // project result can retain source payloads without changing the simple
    // direct include outcome API.
    fn append_include_result(
        &mut self,
        result: IncludeWalkResult,
        complete: &mut bool,
        include_sources: &mut Vec<LoadedSource>,
        cancel: &dyn CancellationToken,
    ) -> Result<(), ResolverError> {
        *complete &= result.complete;
        let old_vector_bytes = result.includes.capacity() * std::mem::size_of::<ResolvedInclude>();
        let additional = (self.project_includes.len() + result.includes.len())
            .saturating_sub(self.project_includes.capacity());
        self.charge_compile_time(
            result.includes.len(),
            0,
            additional * std::mem::size_of::<ResolvedInclude>(),
            cancel,
        )?;
        if additional > 0 {
            self.project_includes.reserve_exact(result.includes.len());
        }
        for include in result.includes {
            if let ResolutionTarget::Found(id) = &include.target
                && !include_sources.iter().any(|existing| existing.id == *id)
            {
                let typed = self
                    .compile_time_caches
                    .iter()
                    .rev()
                    .find_map(|root| {
                        let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
                        cache.loaded.get(id).map(|source| {
                            let mut budget = root.view.store_budget(root.limits, cancel);
                            source.admitted_copy(&mut budget).map_err(root_store_error)
                        })
                    })
                    .transpose()?;
                let source = if let Some(source) = typed {
                    source
                } else if let Some(source) = self.loaded.get(id) {
                    source.clone()
                } else {
                    continue;
                };
                let bytes = if source.compile_time_backing.is_some() {
                    0
                } else {
                    loaded_source_retained(&source)
                };
                self.charge_compile_time(
                    include_sources.len() + 1,
                    0,
                    bytes + std::mem::size_of::<LoadedSource>(),
                    cancel,
                )?;
                include_sources.reserve_exact(1);
                include_sources.push(source);
            }
            self.project_includes.push(include);
        }
        self.release_compile_time(old_vector_bytes);
        Ok(())
    }

    fn record_observation(&mut self, observation: ResolutionObservation) {
        assert!(
            self.compile_time_store.is_none(),
            "typed report construction must precede copying"
        );
        push_observation(&mut self.report.observations, observation);
    }

    fn record_observation_build(
        &mut self,
        payload: usize,
        cancel: &dyn CancellationToken,
        build: impl FnOnce() -> ResolutionObservation,
    ) -> Result<(), ResolverError> {
        if let Some(mut budget) = self.store_budget(cancel) {
            self.report_owners.observations.record(
                &mut self.report.observations,
                &mut budget,
                payload,
                || Ok(build()),
                observation_action,
            )
        } else {
            self.record_observation(build());
            Ok(())
        }
    }

    fn report_message(
        &mut self,
        diagnostic: bool,
        bound: usize,
        args: fmt::Arguments<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<(), ResolverError> {
        if diagnostic {
            self.report.complete = false;
        }
        if !diagnostic && self.limits.max_resolution_warnings == 0 {
            return Ok(());
        }
        let Some(mut budget) = self.store_budget(cancel) else {
            let message = fmt::format(args);
            if diagnostic {
                self.mark_incomplete(message);
            } else {
                self.warn(message);
            }
            return Ok(());
        };
        self.report_message_in_budget(diagnostic, bound, args, &mut budget)
    }

    fn report_message_in_budget(
        &mut self,
        diagnostic: bool,
        bound: usize,
        args: fmt::Arguments<'_>,
        budget: &mut CompileTimeBudget<'_>,
    ) -> Result<(), ResolverError> {
        if diagnostic {
            self.report.complete = false;
        }
        const WARNING_LIMIT: &str = "resolution warning limit reached";
        let bound = bound.max(WARNING_LIMIT.len());
        let (values, owners) = if diagnostic {
            (
                &mut self.report.incomplete_reasons,
                &mut self.report_owners.diagnostics,
            )
        } else {
            (&mut self.report.warnings, &mut self.report_owners.warnings)
        };
        let maximum = self.limits.max_resolution_warnings;
        owners.record(
            values,
            budget,
            bound,
            || {
                let mut writer = BoundedReportString {
                    string: String::with_capacity(bound),
                    bound,
                };
                fmt::write(&mut writer, args).map_err(|_| {
                    ResolverError::InvalidRequest("report formatting bound exceeded".into())
                })?;
                Ok(writer.string)
            },
            |values, message| {
                if values.contains(message) {
                    return ReportAction::Discard;
                }
                if !diagnostic && values.len() >= maximum {
                    if values.last().is_some_and(|s| s == WARNING_LIMIT) {
                        return ReportAction::Discard;
                    }
                    message.clear();
                    message.push_str(WARNING_LIMIT); // admitted same backing
                    ReportAction::Replace(values.len() - 1)
                } else {
                    ReportAction::Append
                }
            },
        )
    }

    fn diagnostic_args(
        &mut self,
        cancel: &dyn CancellationToken,
        bound: usize,
        args: fmt::Arguments<'_>,
    ) -> Result<(), ResolverError> {
        self.report_message(true, bound, args, cancel)
    }
    fn warning_args(
        &mut self,
        cancel: &dyn CancellationToken,
        bound: usize,
        args: fmt::Arguments<'_>,
    ) -> Result<(), ResolverError> {
        self.report_message(false, bound, args, cancel)
    }

    fn warn(&mut self, warning: String) {
        assert!(
            self.compile_time_store.is_none(),
            "typed warning must be admitted before formatting"
        );
        push_warning(
            &mut self.report,
            warning,
            self.limits.max_resolution_warnings,
        );
    }

    fn mark_incomplete(&mut self, reason: String) {
        assert!(
            self.compile_time_store.is_none(),
            "typed diagnostic must be admitted before formatting"
        );
        self.report.complete = false;
        if !self
            .report
            .incomplete_reasons
            .iter()
            .any(|existing| existing == &reason)
        {
            self.report.incomplete_reasons.push(reason);
        }
    }

    fn finish_report(&self) -> ResolutionReport {
        self.report.clone()
    }

    fn take_project_includes(&mut self) -> Vec<ResolvedInclude> {
        std::mem::take(&mut self.project_includes)
    }
}

impl<S: SourceStore> crate::compile_time::CompileTimeResolver for UnitResolver<S> {
    fn include_capability(
        &mut self,
        request: IncludeResolveRequest<'_>,
        budget: &mut CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<AdmittedCapabilityOutcome<LoadedSource>, ResolverError> {
        let mut call = self.callback_report_call();
        call.owning_include_callback = true;
        let outcome =
            crate::compile_time::CompileTimeResolver::include(&mut *call, request, budget, cancel)?;
        call.finish(outcome.result, budget)
    }
    fn unit_report_capability(
        &mut self,
        request: UnitResolveRequest<'_>,
        budget: &mut CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<AdmittedCapabilityOutcome<crate::compile_time::AdmittedProviderSource>, ResolverError>
    {
        let mut call = self.callback_report_call();
        call.owning_unit_callback = true;
        call.owning_unit_reason = true;
        call.owning_unit_candidates = true;
        let outcome = crate::compile_time::CompileTimeResolver::unit_capability(
            &mut *call, request, budget, cancel,
        )?;
        call.finish(outcome.result, budget)
    }
    fn unit_capability(
        &mut self,
        request: UnitResolveRequest<'_>,
        budget: &mut crate::compile_time::CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<ResolutionOutcome<crate::compile_time::AdmittedProviderSource>, ResolverError> {
        if !self.owning_unit_callback {
            // The compatibility capability still returns plain report fields,
            // but its Found provider must use original name owners as well.
            // Scope only the private name handoff; don't replace that report
            // contract with the owning callback's report constructor.
            let mut call = self.callback_report_call();
            call.callback_reports = None;
            call.owning_unit_callback = true;
            return crate::compile_time::CompileTimeResolver::unit_capability(
                &mut *call, request, budget, cancel,
            );
        }
        let outcome =
            crate::compile_time::CompileTimeResolver::unit(self, request, budget, cancel)?;
        if self.owning_unit_callback {
            if let Some(provider) = self.callback_provider.take() {
                if !matches!(&outcome.result,Resolution::Unavailable {reason} if reason.is_empty())
                {
                    return Err(ResolverError::InvalidRequest(
                        "owned provider handoff mismatch".into(),
                    ));
                }
                let backing = crate::compile_time::ProviderSource::admit_cached_backing(
                    &provider.unit.source,
                    budget,
                )?;
                let provider =
                    crate::compile_time::ProviderSource::promote_owned(provider, backing, budget)?;
                return Ok(ResolutionOutcome {
                    result: Resolution::Found(provider),
                    observations: outcome.observations,
                    warnings: outcome.warnings,
                });
            }
        }
        let result = match outcome.result {
            Resolution::Found(_) => {
                return Err(ResolverError::InvalidRequest(
                    "typed provider lost its original name construction owner".into(),
                ));
            }
            Resolution::Unavailable { reason } => Resolution::Unavailable { reason },
            Resolution::Ambiguous { candidates } => Resolution::Ambiguous { candidates },
            Resolution::Incomplete { reason, candidates } => {
                Resolution::Incomplete { reason, candidates }
            }
        };
        Ok(ResolutionOutcome {
            result,
            observations: outcome.observations,
            warnings: outcome.warnings,
        })
    }
    fn max_include_depth(&self) -> usize {
        self.limits.max_include_depth
    }
    fn admit_include(
        &mut self,
        budget: &mut crate::compile_time::CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<bool, ResolverError> {
        self.check_cancel(cancel)?;
        budget
            .charge(1, 0, 0)
            .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
        if self.include_directives >= self.limits.max_include_directives {
            return Ok(false);
        }
        self.include_directives += 1;
        Ok(true)
    }

    fn include(
        &mut self,
        request: IncludeResolveRequest<'_>,
        budget: &mut crate::compile_time::CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<ResolutionOutcome<LoadedSource>, ResolverError> {
        budget
            .charge(1, request.requested_name.len(), 0)
            .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
        self.check_cancel(cancel)?;
        let route = request
            .legacy_route
            .cloned()
            .or_else(|| {
                if self
                    .compile_time_root_path
                    .as_ref()
                    .is_some_and(|p| path_equivalent(p, request.including_path))
                {
                    return self.compile_time_route.clone();
                }
                self.loaded
                    .values()
                    .find(|s| path_equivalent(&s.path, request.including_path))
                    .and_then(|s| self.legacy_routes.get(&s.id))
                    .cloned()
            })
            .or_else(|| {
                self.compile_time_route
                    .as_ref()
                    .filter(|r| path_equivalent(&r.source_path, request.including_path))
                    .cloned()
            });
        // The store's bounded payload read supplements the shared typed budget.
        // Do not reset any resolver counters or construct a child resolver.
        let outcome = self.store_call(budget)?.try_resolve_include(
            IncludeResolveRequest {
                legacy_route: route.as_ref(),
                ..request
            },
            cancel,
        );
        let mut outcome = outcome?;
        if let Resolution::Found(source) = &mut outcome.result {
            if source.decoded_text.is_none() && std::str::from_utf8(&source.bytes).is_err() {
                let payload = source
                    .bytes
                    .len()
                    .checked_mul(3)
                    .and_then(|n| n.checked_add(2 * loaded_source_metadata_retained(source)))
                    .ok_or_else(|| {
                        ResolverError::InvalidRequest("include decode admission overflow".into())
                    })?;
                let owner = budget
                    .admit_owned(1, source.bytes.len(), payload, || {
                        let mut decoded = source.clone();
                        decoded.decoded_text = Some(source_text(source).into());
                        decoded
                    })
                    .and_then(|v| v.into_shared(budget))
                    .and_then(|v| v.into_arc(budget))
                    .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
                *source = (**owner).clone();
                source.compile_time_backing = Some(owner);
                if let Some(root) = self
                    .compile_time_caches
                    .iter()
                    .find(|r| r.view.is_admitted_by(budget))
                {
                    let mut cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(loaded) = cache.value_mut().loaded.get_mut(&source.id) {
                        *loaded = source.admitted_copy(budget).map_err(root_store_error)?;
                    }
                }
            }
            if let Some(route) = route {
                self.legacy_routes.insert(source.id.clone(), route);
            }
        }
        Ok(outcome)
    }

    fn unit(
        &mut self,
        request: UnitResolveRequest<'_>,
        budget: &mut crate::compile_time::CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<ResolutionOutcome<crate::compile_time::ProviderSource>, ResolverError> {
        budget
            .charge(1, request.requested_name.len(), 0)
            .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
        self.check_cancel(cancel)?;
        let route = request
            .legacy_route
            .cloned()
            .or_else(|| {
                if self
                    .compile_time_root_path
                    .as_ref()
                    .is_some_and(|p| path_equivalent(p, request.importer_path))
                {
                    return self.compile_time_route.clone();
                }
                self.loaded
                    .values()
                    .find(|s| path_equivalent(&s.path, request.importer_path))
                    .and_then(|s| self.legacy_routes.get(&s.id))
                    .cloned()
            })
            .or_else(|| {
                self.compile_time_route
                    .as_ref()
                    .filter(|r| path_equivalent(&r.source_path, request.importer_path))
                    .cloned()
            });
        let outcome = self.store_call(budget)?.try_resolve_unit(
            UnitResolveRequest {
                legacy_route: route.as_ref(),
                ..request.clone()
            },
            cancel,
        );
        let outcome = outcome?;
        let result = match outcome.result {
            Resolution::Found(mut unit) => {
                if self.owning_unit_callback {
                    let names = self.callback_unit_names.take().ok_or_else(|| {
                        ResolverError::InvalidRequest(
                            "selected unit has no original name owners".into(),
                        )
                    })?;
                    if !unit.requested_name.is_empty() || !unit.declared_name.is_empty() {
                        return Err(ResolverError::InvalidRequest(
                            "owned unit marker mismatch".into(),
                        ));
                    }
                    let provider = self.build_owned_provider(
                        unit,
                        names,
                        request.requested_name,
                        budget,
                        cancel,
                    )?;
                    self.callback_provider = Some(provider);
                    return Ok(ResolutionOutcome {
                        result: Resolution::Unavailable {
                            reason: String::new(),
                        },
                        observations: outcome.observations,
                        warnings: outcome.warnings,
                    });
                }
                let reserve = conditional_context_reserve(&self.context)?;
                let names = request.requested_name.len()
                    + aliased_name_ref(&self.context, request.requested_name).len()
                    + unit.declared_name.len();
                let payload = reserve
                    + 4 * names
                    + 8 * std::mem::size_of::<String>()
                    + loaded_source_metadata_retained(&unit.source);
                if unit.source.decoded_text.is_none()
                    && std::str::from_utf8(&unit.source.bytes).is_err()
                {
                    unit.source = LoadedSource::admit_construction(
                        budget,
                        unit.source.bytes.len(),
                        unit.source.bytes.len().saturating_mul(3)
                            + loaded_source_metadata_retained(&unit.source),
                        || {
                            let mut decoded = unit.source.clone();
                            decoded.decoded_text = Some(source_text(&unit.source).into());
                            Ok(decoded)
                        },
                    )
                    .map_err(root_store_error)?;
                }
                Resolution::Found(crate::compile_time::ProviderSource::admit_metadata_build(
                    budget,
                    reserve + names,
                    payload,
                    || {
                        let context = self.context.conditional_context_for(&unit.source.path);
                        let qualifiers = qualifiers_for_found(
                            request.requested_name,
                            aliased_name_ref(&self.context, request.requested_name),
                            &unit.declared_name,
                        );
                        crate::compile_time::ProviderSource::new(unit, context, qualifiers)
                    },
                )?)
            }
            Resolution::Unavailable { reason } => Resolution::Unavailable { reason },
            Resolution::Ambiguous { candidates } => Resolution::Ambiguous { candidates },
            Resolution::Incomplete { reason, candidates } => {
                Resolution::Incomplete { reason, candidates }
            }
        };
        Ok(ResolutionOutcome {
            result,
            observations: outcome.observations,
            warnings: outcome.warnings,
        })
    }
}

fn conditional_context_reserve(context: &ProjectContext) -> Result<usize, ResolverError> {
    conditional_context_reserve_polled(context, &NoCancellation)
}

fn conditional_context_reserve_polled(
    context: &ProjectContext,
    cancel: &dyn CancellationToken,
) -> Result<usize, ResolverError> {
    let mut total = std::mem::size_of::<pascal_project::ConditionalContext>();
    for candidate in [
        &context.conditional_context,
        &context.library_conditional_context,
    ] {
        let mut cancelled = false;
        candidate
            .visit_recovery_payload(&mut |n| {
                if cancel.is_cancelled() {
                    cancelled = true;
                    return Err("context sizing cancelled".into());
                }
                total = total
                    .checked_add(n)
                    .ok_or_else(|| "context admission overflow".to_string())?;
                Ok(())
            })
            .map_err(|error| {
                if cancelled {
                    ResolverError::Cancelled
                } else {
                    ResolverError::InvalidRequest(error)
                }
            })?;
        total = total
            .checked_add(
                (candidate.defines.len() + candidate.options.len() + candidate.constants.len())
                    * (std::mem::size_of::<(String, crate::conditional::ConstantValue)>()
                        + 4 * std::mem::size_of::<usize>()),
            )
            .ok_or_else(|| ResolverError::InvalidRequest("context admission overflow".into()))?;
    }
    for define in &context.defines {
        if cancel.is_cancelled() {
            return Err(ResolverError::Cancelled);
        }
        total = total
            .checked_add(
                define.len()
                    + std::mem::size_of::<(String, crate::conditional::Truth)>()
                    + 4 * std::mem::size_of::<usize>(),
            )
            .ok_or_else(|| ResolverError::InvalidRequest("context admission overflow".into()))?;
    }
    Ok(total)
}

struct CandidateGroup<'a> {
    paths: LookupScratch<Vec<(PathBuf, ProjectPathEntry)>>,
    legacy_route: Option<&'a LegacyRoute>,
}

fn explicit_path_vector(
    count: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<AdmittedOwned<Vec<(PathBuf, ProjectPathEntry)>>, ResolverError> {
    let slots = count
        .checked_mul(std::mem::size_of::<(PathBuf, ProjectPathEntry)>())
        .ok_or_else(|| ResolverError::InvalidRequest("explicit vector capacity overflow".into()))?;
    budget
        .admit_owned(count, 0, slots, || Vec::with_capacity(count))
        .map_err(|r| crate::compile_time::resolver_error(r, budget))
}

fn explicit_path_push(
    to: &mut AdmittedOwned<Vec<(PathBuf, ProjectPathEntry)>>,
    path: &Path,
    entry: AdmittedOwned<ProjectPathEntry>,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<(), ResolverError> {
    if !to.is_admitted_by(budget) || !entry.is_admitted_by(budget) || to.len() == to.capacity() {
        return Err(ResolverError::InvalidRequest(
            "explicit receiving owner mismatch".into(),
        ));
    }
    let path = include_limit_path_copy(path, budget).map_err(root_store_error)?;
    let mut pair = path
        .map_value(budget, |path| {
            (path, ProjectPathEntry::legacy(PathBuf::new()))
        })
        .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
    pair.move_field(entry, budget, |pair, entry| pair.1 = entry)
        .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
    to.move_field(pair, budget, |to, pair| to.push(pair))
        .map_err(|r| crate::compile_time::resolver_error(r, budget))
}

/// Private temporary storage only. Deref borrows cannot detach its payload from
/// the admission; escaping result fields require a separate consuming owner.
enum LookupScratch<T> {
    Legacy(T),
    Admitted(AdmittedOwned<T>),
}
impl<T> From<T> for LookupScratch<T> {
    fn from(value: T) -> Self {
        Self::Legacy(value)
    }
}
impl<T> std::ops::Deref for LookupScratch<T> {
    type Target = T;
    fn deref(&self) -> &T {
        match self {
            Self::Legacy(value) => value,
            Self::Admitted(value) => value,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DirectoryCacheKey {
    /// `path_lookup_key` of the directory: one entry for every spelling of it.
    path: PathBuf,
    entry: ProjectPathEntry,
    read_policy: ReadPolicy,
    missing_is_incomplete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct UnitCacheKey {
    importer_directory: PathBuf,
    requested_name: String,
    legacy_route: Option<LegacyRoute>,
}

impl UnitCacheKey {
    fn new(path: &Path, requested_name: &str, legacy_route: Option<&LegacyRoute>) -> Self {
        Self {
            importer_directory: canonical_path(path.parent().unwrap_or(path)),
            requested_name: requested_name.to_ascii_lowercase(),
            legacy_route: legacy_route.cloned(),
        }
    }
}

#[derive(Debug, Clone)]
struct Catalogue {
    entries: HashMap<String, Vec<PathBuf>>,
    complete: bool,
}

const MAX_SHARED_PACKAGE_CATALOGUES: usize = 16;

/// Bounded cache for resolver instances sharing one project context and the
/// same limits during a single operation. Discard this cache when the
/// operation completes.
#[derive(Debug, Clone, Default)]
pub struct ResolverSessionCache {
    state: Arc<Mutex<ResolverSessionCacheState>>,
}

#[derive(Debug, Default)]
struct ResolverSessionCacheState {
    scope: Option<ResolverSessionCacheScope>,
    catalogues: HashMap<PathBuf, CachedCatalogue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolverSessionCacheScope {
    context: ProjectContext,
    limits: ResolverLimits,
}

#[derive(Debug, Clone)]
struct CachedCatalogue {
    catalogue: Arc<Catalogue>,
    observations: Arc<[ResolutionObservation]>,
    warnings: Arc<[String]>,
    incomplete_reasons: Arc<[String]>,
}

impl ResolverSessionCache {
    fn bind(&self, context: &ProjectContext, limits: ResolverLimits) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &state.scope {
            Some(bound_scope) => bound_scope.limits == limits && &bound_scope.context == context,
            None => {
                state.scope = Some(ResolverSessionCacheScope {
                    context: context.clone(),
                    limits,
                });
                true
            }
        }
    }

    fn get(&self, root: &Path) -> Option<CachedCatalogue> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .catalogues
            .get(root)
            .cloned()
    }

    fn insert(&self, root: PathBuf, catalogue: CachedCatalogue, maximum: usize) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let catalogues = &mut state.catalogues;
        let maximum = maximum.min(MAX_SHARED_PACKAGE_CATALOGUES);
        if catalogues.contains_key(&root) || catalogues.len() < maximum {
            catalogues.entry(root).or_insert(catalogue);
        }
    }
}

enum CaseInsensitiveLookup {
    Missing,
    Found(LookupScratch<PathBuf>),
    Ambiguous(LookupScratch<Vec<PathBuf>>),
    Incomplete,
}

#[derive(Debug, Clone, Default)]
struct IncludeWalkResult {
    includes: Vec<ResolvedInclude>,
    complete: bool,
}

struct SourceOrderProjection {
    root: crate::conditional::ConditionalAnalysis,
    retained: usize,
    includes: Option<IncludeWalkResult>,
}

struct QueuedProjectImporter {
    unit: ResolvedUnit,
    sites: Vec<ImportSite>,
    site_retained: usize,
}

/// Includes the currently popped/consumed site's reservation until its buffer
/// drops; backing queue capacity stays reserved until the whole walk finishes.
#[derive(Default)]
struct ProjectImportQueue {
    items: VecDeque<QueuedProjectImporter>,
    site_retained: usize,
    storage_retained: usize,
}

fn conditional_analysis_retained(analysis: &crate::conditional::ConditionalAnalysis) -> usize {
    use std::mem::size_of;
    size_of::<crate::conditional::ConditionalAnalysis>()
        .saturating_add(analysis.projected_source.capacity())
        .saturating_add(
            (analysis.inactive_spans.capacity() + analysis.unknown_spans.capacity())
                * size_of::<Range<usize>>(),
        )
        .saturating_add(
            analysis.directives.capacity() * size_of::<crate::conditional::ConditionalDirective>(),
        )
        .saturating_add(
            analysis
                .directives
                .iter()
                .map(|d| d.body.capacity())
                .sum::<usize>(),
        )
}

pub(crate) fn loaded_source_metadata_retained(source: &LoadedSource) -> usize {
    let mut bytes = std::mem::size_of::<LoadedSource>()
        + source.id.as_str().len()
        + source.path.as_os_str().len();
    if let SourceRevision::Disk {
        read_policy,
        path_entry,
        ..
    } = &source.revision
    {
        let mut count = |n| {
            bytes = bytes.saturating_add(n);
            Ok(())
        };
        read_policy
            .visit_owned_clone_payload(&mut count)
            .expect("infallible size visitor");
        path_entry
            .visit_recovery_payload(&mut count)
            .expect("infallible size visitor");
    }
    bytes
}

fn loaded_source_retained(source: &LoadedSource) -> usize {
    loaded_source_metadata_retained(source)
        .saturating_add(source.bytes.len())
        .saturating_add(source.decoded_text.as_ref().map_or(0, |text| text.len()))
}

#[derive(Debug, Clone)]
struct ParsedSourceMetadata {
    declared_name: Option<String>,
    imports: Vec<ImportSite>,
}

/// Parsed metadata for one loaded source. The parser inputs are retained so a
/// reload with different content under the same ID is never served stale
/// metadata.
#[derive(Debug, Clone)]
struct CachedSourceMetadata {
    bytes: Arc<[u8]>,
    decoded_text: Option<Arc<str>>,
    metadata: Arc<ParsedSourceMetadata>,
}

impl CachedSourceMetadata {
    fn matches(&self, source: &LoadedSource) -> bool {
        (Arc::ptr_eq(&self.bytes, &source.bytes) || self.bytes == source.bytes)
            && self.decoded_text == source.decoded_text
    }
}

fn source_text(source: &LoadedSource) -> String {
    source.decoded_text.as_deref().map_or_else(
        || crate::text::decode_bytes(&source.bytes).into_owned(),
        ToOwned::to_owned,
    )
}

#[cfg(test)]
thread_local! {
    static TEST_METADATA_PARSES: std::cell::RefCell<HashMap<PathBuf, usize>> =
        std::cell::RefCell::new(HashMap::new());
}

#[cfg(test)]
fn reset_test_metadata_parses() {
    TEST_METADATA_PARSES.with(|parses| parses.borrow_mut().clear());
}

#[cfg(test)]
fn test_metadata_parses(path: &Path) -> usize {
    TEST_METADATA_PARSES.with(|parses| parses.borrow().get(path).copied().unwrap_or(0))
}

fn parse_source_metadata(source: &LoadedSource) -> ParsedSourceMetadata {
    #[cfg(test)]
    TEST_METADATA_PARSES.with(|parses| {
        *parses.borrow_mut().entry(source.path.clone()).or_default() += 1;
    });
    let parse_bytes = source
        .decoded_text
        .as_deref()
        .map_or(source.bytes.as_ref(), str::as_bytes);
    let (tree, _) = match crate::parser::parse_file(
        &crate::types::FileInfo::new(source.path.clone()),
        parse_bytes,
    ) {
        Ok(parsed) => parsed,
        Err(_) => {
            return ParsedSourceMetadata {
                declared_name: None,
                imports: Vec::new(),
            };
        }
    };
    let mut modules = Vec::new();
    collect_nodes(tree.root_node(), &mut modules);
    let unit = modules
        .iter()
        .find(|node| !has_ancestor_kind(**node, "declUses"));
    let declared_name = unit
        .and_then(|node| node.utf8_text(parse_bytes).ok())
        .map(canonical_unit_name)
        .filter(|name| !name.is_empty());
    let mut imports = Vec::new();
    for node in modules
        .iter()
        .filter(|node| has_ancestor_kind(**node, "declUses"))
    {
        let Ok(name) = node.utf8_text(parse_bytes) else {
            continue;
        };
        let name = canonical_unit_name(name);
        if name.is_empty() {
            continue;
        }
        let section = if has_ancestor_kind(*node, "interface") {
            ImportSection::Interface
        } else if has_ancestor_kind(*node, "implementation") {
            ImportSection::Implementation
        } else {
            ImportSection::Module
        };
        imports.push(ImportSite {
            byte_range: node.start_byte()..node.end_byte(),
            requested_name: name,
            section,
        });
    }
    ParsedSourceMetadata {
        declared_name,
        imports,
    }
}

fn collect_nodes<'a>(node: tree_sitter::Node<'a>, output: &mut Vec<tree_sitter::Node<'a>>) {
    if node.kind() == "moduleName" {
        output.push(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_nodes(child, output);
    }
}

fn has_ancestor_kind(mut node: tree_sitter::Node<'_>, kind: &str) -> bool {
    while let Some(parent) = node.parent() {
        if parent.kind() == kind {
            return true;
        }
        node = parent;
    }
    false
}

fn declared_name_matches_owned(
    actual: Option<&str>,
    requested: &str,
    lookup: &str,
    context: &ProjectContext,
    budget: &mut CompileTimeBudget<'_>,
    cancel: &dyn CancellationToken,
) -> Result<bool, ResolverError> {
    let Some(actual) = actual else {
        return Ok(false);
    };
    let overflow = || ResolverError::InvalidRequest("candidate name matching work overflow".into());
    budget
        .charge(
            context
                .unit_namespaces
                .len()
                .checked_add(2)
                .ok_or_else(overflow)?,
            actual
                .len()
                .checked_mul(2)
                .and_then(|n| n.checked_add(requested.len()))
                .and_then(|n| n.checked_add(lookup.len()))
                .ok_or_else(overflow)?,
            0,
        )
        .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
    if cancel.is_cancelled() {
        return Err(ResolverError::Cancelled);
    }
    if actual.eq_ignore_ascii_case(requested) || actual.eq_ignore_ascii_case(lookup) {
        return Ok(true);
    }
    if lookup.contains('.') {
        return Ok(false);
    }
    for namespace in &context.unit_namespaces {
        if cancel.is_cancelled() {
            return Err(ResolverError::Cancelled);
        }
        budget
            .charge(
                1,
                namespace
                    .len()
                    .checked_add(lookup.len())
                    .and_then(|n| n.checked_add(1))
                    .ok_or_else(overflow)?,
                0,
            )
            .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
        if actual
            .get(..namespace.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(namespace))
            && actual
                .get(namespace.len()..)
                .and_then(|tail| tail.strip_prefix('.'))
                .is_some_and(|tail| tail.eq_ignore_ascii_case(lookup))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn declared_name_matches(
    actual: Option<&str>,
    requested: &str,
    lookup: &str,
    context: &ProjectContext,
) -> bool {
    let Some(actual) = actual else {
        return false;
    };
    actual.eq_ignore_ascii_case(requested)
        || actual.eq_ignore_ascii_case(lookup)
        || (!lookup.contains('.')
            && context
                .unit_namespaces
                .iter()
                .any(|namespace| format!("{namespace}.{lookup}").eq_ignore_ascii_case(actual)))
}

fn qualifiers_for_found(requested: &str, lookup: &str, declared: &str) -> Vec<String> {
    // Five borrowed spellings at most; avoid a hidden old-plus-new growth
    // peak inside the preadmitted provider construction.
    let mut qualifiers = Vec::with_capacity(5);
    for spelling in [
        Some(requested),
        (!lookup.eq_ignore_ascii_case(requested)).then_some(lookup),
        (!declared.eq_ignore_ascii_case(requested) && !declared.eq_ignore_ascii_case(lookup))
            .then_some(declared),
        short_unit_name(lookup),
        short_unit_name(declared),
    ] {
        if let Some(spelling) = spelling.filter(|spelling| !spelling.trim().is_empty())
            && !qualifiers
                .iter()
                .any(|existing: &String| existing.eq_ignore_ascii_case(spelling))
        {
            qualifiers.push(spelling.to_string());
        }
    }
    qualifiers
}

fn qualifiers_for_unresolved(requested: &str, lookup: &str) -> Vec<String> {
    let mut qualifiers = Vec::new();
    for spelling in [
        Some(requested),
        (!lookup.eq_ignore_ascii_case(requested)).then_some(lookup),
    ] {
        if let Some(spelling) = spelling.filter(|spelling| !spelling.trim().is_empty())
            && !qualifiers
                .iter()
                .any(|existing: &String| existing.eq_ignore_ascii_case(spelling))
        {
            qualifiers.push(spelling.to_string());
        }
    }
    qualifiers
}

fn short_unit_name(name: &str) -> Option<&str> {
    name.rsplit('.').next().filter(|short| *short != name)
}

fn analysis_contains_range(spans: &[Range<usize>], range: &Range<usize>) -> bool {
    spans
        .iter()
        .any(|span| span.start <= range.start && range.end <= span.end)
}

fn aliased_name(context: &ProjectContext, requested: &str) -> String {
    aliased_name_ref(context, requested).to_owned()
}

fn aliased_name_ref<'a>(context: &'a ProjectContext, requested: &'a str) -> &'a str {
    context
        .unit_aliases
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(requested))
        .map_or(requested, |(_, target)| target.as_str())
}

fn filename_tiers(unit_name: &str, namespaces: &[String]) -> Vec<Vec<String>> {
    let short = unit_name.rsplit('.').next().unwrap_or(unit_name);
    let mut exact = vec![format!("{unit_name}.pas")];
    if !short.eq_ignore_ascii_case(unit_name) {
        exact.push(format!("{short}.pas"));
    }
    let mut tiers = vec![exact];
    if !unit_name.contains('.') {
        for namespace in namespaces {
            let namespace = namespace.trim().trim_matches('.');
            if !namespace.is_empty() {
                tiers.push(vec![format!("{namespace}.{unit_name}.pas")]);
            }
        }
    }
    for tier in &mut tiers {
        tier.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
    }
    tiers
}

pub(crate) fn include_name(body: &str) -> Option<String> {
    let raw = body
        .trim_start()
        .split_once(|character: char| character.is_ascii_whitespace() || character == ':')
        .map(|(_, rest)| rest.trim())
        .filter(|rest| !rest.is_empty())?;
    if raw.contains("$(") {
        return None;
    }
    let raw = if let Some(quoted) = raw.strip_prefix('\'') {
        quoted.strip_suffix('\'')?
    } else if let Some(quoted) = raw.strip_prefix('"') {
        quoted.strip_suffix('"')?
    } else {
        if raw
            .chars()
            .any(|character| character == '\'' || character == '"')
        {
            return None;
        }
        raw
    };
    (!raw.is_empty()).then(|| raw.replace('\\', "/"))
}

fn canonical_unit_name(name: &str) -> String {
    name.trim()
        .replace(char::is_whitespace, "")
        .trim_matches('.')
        .to_string()
}

/// Borrow-only unit-header metadata for the admitted lookup. In particular it
/// does not invoke the legacy whole-file tree-sitter parser before root admission.
/// Directives before the header cannot prove an unconditional unit identity.
fn admitted_header_name(bytes: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(bytes)
        .ok()?
        .trim_start_matches('\u{feff}');
    let mut rest = text;
    loop {
        rest = rest.trim_start();
        if let Some(comment) = rest.strip_prefix("//") {
            rest = comment.split_once('\n')?.1;
        } else if let Some(comment) = rest.strip_prefix('{') {
            if comment.starts_with('$') {
                return None;
            }
            rest = comment.split_once('}')?.1;
        } else if let Some(comment) = rest.strip_prefix("(*") {
            if comment.starts_with('$') {
                return None;
            }
            rest = comment.split_once("*)")?.1;
        } else {
            break;
        }
    }
    let head = rest.get(..4)?;
    if !head.eq_ignore_ascii_case("unit") || !rest.get(4..)?.starts_with(char::is_whitespace) {
        return None;
    }
    let name = rest.get(4..)?.split_once(';')?.0.trim();
    let mut start = true;
    for c in name.chars().filter(|c| !c.is_whitespace()) {
        if c == '.' {
            if start {
                return None;
            }
            start = true;
        } else if c == '_' || c.is_alphabetic() || (!start && c.is_ascii_digit()) {
            start = false;
        } else {
            return None;
        }
    }
    (!start).then_some(name)
}

fn candidate_vector(
    count: usize,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<AdmittedOwned<Vec<ResolutionCandidate>>, ResolverError> {
    let bytes = count
        .checked_mul(std::mem::size_of::<ResolutionCandidate>())
        .ok_or_else(|| {
            ResolverError::InvalidRequest("unit candidate vector capacity overflow".into())
        })?;
    budget
        .admit_owned(1, 0, bytes, || Vec::with_capacity(count))
        .map_err(|r| crate::compile_time::resolver_error(r, budget))
}

fn unit_candidate_entry(
    path: &Path,
    name: Option<AdmittedOwned<String>>,
    budget: &mut CompileTimeBudget<'_>,
    cancel: &dyn CancellationToken,
) -> Result<AdmittedOwned<ResolutionCandidate>, ResolverError> {
    if name
        .as_ref()
        .is_some_and(|name| !name.is_admitted_by(budget))
    {
        return Err(ResolverError::InvalidRequest(
            "foreign-root candidate declared name".into(),
        ));
    }
    let path = budget.admit_fallible(
        1,
        path.as_os_str().len(),
        path.as_os_str().len(),
        crate::compile_time::resolver_error,
        || {
            if cancel.is_cancelled() {
                return Err(ResolverError::Cancelled);
            }
            let path = path.to_path_buf();
            if cancel.is_cancelled() {
                return Err(ResolverError::Cancelled);
            }
            Ok(path)
        },
    )?;
    let mut candidate = path
        .map_value(budget, |path| ResolutionCandidate {
            path,
            declared_name: None,
        })
        .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
    if let Some(name) = name {
        candidate
            .move_field(name, budget, |candidate, name| {
                candidate.declared_name = Some(name)
            })
            .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
    }
    Ok(candidate)
}

fn push_unit_candidate(
    vector: &mut AdmittedOwned<Vec<ResolutionCandidate>>,
    candidate: AdmittedOwned<ResolutionCandidate>,
    budget: &CompileTimeBudget<'_>,
) -> Result<(), ResolverError> {
    if vector.len() == vector.capacity() {
        return Err(ResolverError::InvalidRequest(
            "candidate push exceeds admitted receiving capacity".into(),
        ));
    }
    vector
        .move_field(candidate, budget, |vector, candidate| {
            vector.push(candidate)
        })
        .map_err(|r| crate::compile_time::resolver_error(r, budget))
}

fn build_unit_reason(
    budget: &mut CompileTimeBudget<'_>,
    bound: usize,
    cancel: &dyn CancellationToken,
    args: std::fmt::Arguments<'_>,
) -> Result<AdmittedOwned<String>, ResolverError> {
    budget.admit_fallible(1, bound, bound, crate::compile_time::resolver_error, || {
        use std::fmt::Write;
        if cancel.is_cancelled() {
            return Err(ResolverError::Cancelled);
        }
        let mut reason = String::with_capacity(bound);
        reason.write_fmt(args).expect("unit reason formatting");
        if cancel.is_cancelled() {
            return Err(ResolverError::Cancelled);
        }
        Ok(reason)
    })
}

fn owned_header_name(
    bytes: &[u8],
    budget: &mut CompileTimeBudget<'_>,
    cancel: &dyn CancellationToken,
) -> Result<Option<AdmittedOwned<String>>, ResolverError> {
    let work = bytes
        .len()
        .checked_mul(8)
        .ok_or_else(|| ResolverError::InvalidRequest("header parsing work overflow".into()))?;
    // CPU parsing is admitted before UTF-8 validation or header scanning. No
    // whole-source parser/cache receipt is carried by the selected name.
    let _parse = budget
        .admit_owned(work, bytes.len(), 0, || ())
        .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
    for _ in bytes.chunks(256) {
        if cancel.is_cancelled() {
            return Err(ResolverError::Cancelled);
        }
    }
    let Some(name) = header_name_polled(bytes, cancel)? else {
        return Ok(None);
    };
    let name = budget.admit_fallible(
        name.len(),
        name.len(),
        name.len(),
        crate::compile_time::resolver_error,
        || {
            let mut output = String::with_capacity(name.len());
            for character in name.chars() {
                if cancel.is_cancelled() {
                    return Err(ResolverError::Cancelled);
                }
                if !character.is_whitespace() {
                    output.push(character);
                }
            }
            // Validated header names have no leading/trailing dot; this preserves
            // canonical_unit_name without its transient replace/trim/copy buffers.
            Ok(output)
        },
    )?;
    Ok(Some(name))
}

fn header_name_polled<'a>(
    bytes: &'a [u8],
    cancel: &dyn CancellationToken,
) -> Result<Option<&'a str>, ResolverError> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Ok(None);
    };
    fn trim_start<'a>(
        text: &'a str,
        cancel: &dyn CancellationToken,
    ) -> Result<&'a str, ResolverError> {
        for (index, character) in text.char_indices() {
            if cancel.is_cancelled() {
                return Err(ResolverError::Cancelled);
            }
            if !character.is_whitespace() {
                return Ok(&text[index..]);
            }
        }
        Ok("")
    }
    fn split<'a>(
        text: &'a str,
        needle: &str,
        cancel: &dyn CancellationToken,
    ) -> Result<Option<(&'a str, &'a str)>, ResolverError> {
        for (index, window) in text.as_bytes().windows(needle.len()).enumerate() {
            if cancel.is_cancelled() {
                return Err(ResolverError::Cancelled);
            }
            if window == needle.as_bytes() {
                return Ok(Some((&text[..index], &text[index + needle.len()..])));
            }
        }
        Ok(None)
    }
    let mut rest = text.trim_start_matches('\u{feff}');
    loop {
        rest = trim_start(rest, cancel)?;
        let (comment, end) = if let Some(comment) = rest.strip_prefix("//") {
            (comment, "\n")
        } else if let Some(comment) = rest.strip_prefix('{') {
            if comment.starts_with('$') {
                return Ok(None);
            }
            (comment, "}")
        } else if let Some(comment) = rest.strip_prefix("(*") {
            if comment.starts_with('$') {
                return Ok(None);
            }
            (comment, "*)")
        } else {
            break;
        };
        let Some((_, after)) = split(comment, end, cancel)? else {
            return Ok(None);
        };
        rest = after;
    }
    let Some(head) = rest.get(..4) else {
        return Ok(None);
    };
    if !head.eq_ignore_ascii_case("unit")
        || !rest
            .get(4..)
            .is_some_and(|tail| tail.starts_with(char::is_whitespace))
    {
        return Ok(None);
    }
    let Some((name, _)) = split(&rest[4..], ";", cancel)? else {
        return Ok(None);
    };
    let name = name.trim();
    let mut start = true;
    for character in name.chars() {
        if cancel.is_cancelled() {
            return Err(ResolverError::Cancelled);
        }
        if character.is_whitespace() {
            continue;
        }
        if character == '.' {
            if start {
                return Ok(None);
            }
            start = true;
        } else if character == '_'
            || character.is_alphabetic()
            || (!start && character.is_ascii_digit())
        {
            start = false;
        } else {
            return Ok(None);
        }
    }
    Ok((!start).then_some(name))
}

fn package_stem(name: &str) -> String {
    let name = name.replace('\\', "/");
    let name = name.rsplit('/').next().unwrap_or(&name);
    name.rsplit_once('.')
        .map_or(name, |(stem, extension)| {
            if matches!(
                extension.to_ascii_lowercase().as_str(),
                "dcp" | "dpk" | "dproj" | "bpl"
            ) {
                stem
            } else {
                name
            }
        })
        .to_ascii_lowercase()
}

fn push_warning(report: &mut ResolutionReport, warning: String, maximum: usize) {
    if report.warnings.iter().any(|existing| existing == &warning) {
        return;
    }
    if maximum == 0 {
        return;
    }
    if report.warnings.len() < maximum {
        report.warnings.push(warning);
    } else if report
        .warnings
        .last()
        .is_none_or(|last| last != "resolution warning limit reached")
        && let Some(last) = report.warnings.last_mut()
    {
        *last = "resolution warning limit reached".to_string();
    }
}

fn push_observation(
    observations: &mut Vec<ResolutionObservation>,
    observation: ResolutionObservation,
) {
    match &observation {
        ResolutionObservation::Metadata(metadata) => {
            let path = metadata.path();
            if let Some(existing) = observations.iter_mut().find(|existing| matches!(existing, ResolutionObservation::Metadata(current) if current.path() == path)) {
                if matches!(existing, ResolutionObservation::Metadata(MetadataObservation::Stat { .. }))
                    && matches!(metadata, MetadataObservation::Payload { .. })
                { *existing = observation; }
                return;
            }
        }
        ResolutionObservation::ProjectRead(read) => {
            if observations.iter().any(|existing| matches!(existing, ResolutionObservation::ProjectRead(current) if current.path == read.path)) { return; }
        }
        ResolutionObservation::Candidate { path, present, .. } => {
            if let Some(existing) = observations.iter_mut().find(|existing| {
                matches!(existing, ResolutionObservation::Candidate { path: current, .. } if current == path)
            }) {
                if *present {
                    *existing = observation;
                }
                return;
            }
        }
        _ => {
            if observations.iter().any(|existing| existing == &observation) { return; }
        }
    }
    observations.push(observation);
}

fn source_request_path(
    path: &Path,
    budget: &mut CompileTimeBudget<'_>,
    cancel: &dyn CancellationToken,
) -> Result<AdmittedOwned<PathBuf>, SourceStoreError> {
    // Production callers already canonicalize their query/group paths. Relative
    // cwd/query construction belongs to those caller boundaries, not this local
    // request. Keep the plain legacy constructor compatible below.
    let bytes = path.as_os_str().len();
    let work = bytes
        .checked_mul(4)
        .and_then(|n| n.checked_add(1))
        .ok_or(SourceStoreError::AdmissionUnavailable)?;
    budget.admit_fallible(work, bytes, bytes, store_admission_error, || {
        if cancel.is_cancelled() {
            return Err(SourceStoreError::Cancelled);
        }
        // Prefix/root validation can traverse a platform-specific prefix; do
        // it only after the byte-bound construction work has been admitted.
        if !path.is_absolute() && !path.has_root() {
            return Err(SourceStoreError::AdmissionUnavailable);
        }
        let mut normalized = PathBuf::with_capacity(bytes);
        for component in path.components() {
            if cancel.is_cancelled() {
                return Err(SourceStoreError::Cancelled);
            }
            match component {
                Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
                Component::RootDir => normalized.push(std::path::MAIN_SEPARATOR_STR),
                Component::CurDir => {}
                Component::ParentDir => {
                    normalized.pop();
                }
                Component::Normal(value) => normalized.push(value),
            }
        }
        if cancel.is_cancelled() {
            return Err(SourceStoreError::Cancelled);
        }
        Ok(normalized)
    })
}

fn source_request_entry(
    path: &Path,
    entry: &ProjectPathEntry,
    budget: &mut CompileTimeBudget<'_>,
    cancel: &dyn CancellationToken,
) -> Result<AdmittedOwned<ProjectPathEntry>, SourceStoreError> {
    // Enum payload shape is fixed: at most one mapped-root PathBuf. These are
    // O(1) byte bounds, not a hidden provenance traversal before admission.
    let provenance = match &entry.provenance {
        ProjectPathProvenance::Mapped { root } => root.as_os_str().len(),
        _ => 0,
    };
    let bytes = path
        .as_os_str()
        .len()
        .checked_add(provenance)
        .ok_or(SourceStoreError::AdmissionUnavailable)?;
    budget.admit_fallible(1, bytes, bytes, store_admission_error, || {
        if cancel.is_cancelled() {
            return Err(SourceStoreError::Cancelled);
        }
        let local = ProjectPathEntry {
            path: path.to_path_buf(),
            provenance: entry.provenance.clone(),
        };
        if cancel.is_cancelled() {
            return Err(SourceStoreError::Cancelled);
        }
        Ok(local)
    })
}

struct IncludeQuery {
    canonical: AdmittedOwned<PathBuf>,
    id: AdmittedOwned<SourceId>,
    identity: AdmittedOwned<String>,
}

enum IncludeLossy<'a> {
    Borrowed(&'a str),
    Admitted(AdmittedOwned<String>),
}
impl std::ops::Deref for IncludeLossy<'_> {
    type Target = str;
    fn deref(&self) -> &str {
        match self {
            Self::Borrowed(text) => text,
            Self::Admitted(text) => text,
        }
    }
}
fn include_lossy<'a>(
    path: &'a Path,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<IncludeLossy<'a>, SourceStoreError> {
    let n = path.as_os_str().len();
    let work = n
        .checked_add(1)
        .ok_or(SourceStoreError::AdmissionUnavailable)?;
    budget
        .charge(work, n, 0)
        .map_err(|r| store_admission_error(r, budget))?;
    let text = path.to_str();
    if budget.cancel_token().is_cancelled() {
        return Err(SourceStoreError::Cancelled);
    }
    if let Some(text) = text {
        return Ok(IncludeLossy::Borrowed(text));
    }
    // Expansion is at most three UTF-8 bytes per input byte. Allow the
    // standard String builder's doubling capacity and small-buffer minimum.
    let bound = n
        .checked_mul(6)
        .map(|n| n.max(8))
        .ok_or(SourceStoreError::AdmissionUnavailable)?;
    budget
        .admit_owned(work, n, bound, || path.to_string_lossy().into_owned())
        .map(IncludeLossy::Admitted)
        .map_err(|r| store_admission_error(r, budget))
}

fn include_query(
    path: &Path,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<IncludeQuery, SourceStoreError> {
    let cancel = budget.cancel_token();
    let canonical = source_request_path(path, budget, cancel)?;
    let lossy = include_lossy(&canonical, budget)?;
    let bytes = lossy
        .len()
        .checked_add(7)
        .ok_or(SourceStoreError::AdmissionUnavailable)?;
    let id = budget
        .admit_owned(1, bytes, bytes, || {
            let mut id = String::with_capacity(bytes);
            id.push_str("source:");
            id.push_str(&lossy);
            SourceId::new(id)
        })
        .map_err(|r| store_admission_error(r, budget))?;
    drop(lossy);
    let identity = include_identity(&canonical, budget)?;
    Ok(IncludeQuery {
        canonical,
        id,
        identity,
    })
}

/// Already canonical input; independently own lossy scratch and the transformed
/// key rather than charging a hidden path_key normalization/replace chain.
fn include_identity(
    path: &Path,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<AdmittedOwned<String>, SourceStoreError> {
    let cancel = budget.cancel_token();
    let bound = path
        .as_os_str()
        .len()
        .checked_mul(3)
        .ok_or(SourceStoreError::AdmissionUnavailable)?;
    let work = bound
        .checked_add(1)
        .ok_or(SourceStoreError::AdmissionUnavailable)?;
    let lossy = include_lossy(path, budget)?;
    budget.admit_fallible(
        work,
        lossy.len(),
        lossy.len(),
        store_admission_error,
        || {
            let insensitive = path_identity::is_case_insensitive(path);
            let mut key = String::with_capacity(lossy.len());
            for (index, ch) in lossy.chars().enumerate() {
                if index % 8 == 0 && cancel.is_cancelled() {
                    return Err(SourceStoreError::Cancelled);
                }
                key.push(if ch == '\\' {
                    '/'
                } else if insensitive {
                    ch.to_ascii_lowercase()
                } else {
                    ch
                });
            }
            if cancel.is_cancelled() {
                return Err(SourceStoreError::Cancelled);
            }
            Ok(key)
        },
    )
}

fn canonical_path(path: &Path) -> PathBuf {
    // A rooted Windows path without a drive (`\workspace`) keeps its form, so
    // it still matches the project paths it was compared with.
    let absolute = if path.is_absolute() || path.has_root() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(std::path::MAIN_SEPARATOR.to_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    // Keep the spelling: these paths are opened, reported and compared with
    // the caller's paths. Case-insensitive identity is `path_key`'s job.
    normalized
}

fn source_id_for_path(path: &Path) -> SourceId {
    SourceId::new(format!("source:{}", canonical_path(path).to_string_lossy()))
}

fn legacy_route_authorizes(path: &Path, route: &LegacyRoute) -> bool {
    let path = canonical_path(path);
    let source = canonical_path(&route.source_path);
    let sibling_directory = canonical_path(&route.sibling_directory);
    source
        .parent()
        .is_some_and(|parent| path_equivalent(parent, &sibling_directory))
        && path
            .parent()
            .is_some_and(|parent| path_equivalent(parent, &sibling_directory))
}

fn legacy_route_authorizes_directory(directory: &Path, route: &LegacyRoute) -> bool {
    let directory = canonical_path(directory);
    let source = canonical_path(&route.source_path);
    let sibling_directory = canonical_path(&route.sibling_directory);
    source
        .parent()
        .is_some_and(|parent| path_equivalent(parent, &sibling_directory))
        && path_equivalent(&directory, &sibling_directory)
}

fn copy_case_path(
    path: &PathBuf,
    budget: &mut CompileTimeBudget<'_>,
) -> Result<AdmittedOwned<PathBuf>, ResolverError> {
    let bytes = path.as_os_str().len();
    budget
        .admit_owned(1, bytes, bytes, || path.clone())
        .map_err(|r| crate::compile_time::resolver_error(r, budget))
}

fn case_sort_sizing(
    listing: &DirectoryListing,
    wanted: &std::ffi::OsStr,
    budget: &mut CompileTimeBudget<'_>,
    cancel: &dyn CancellationToken,
) -> Result<usize, ResolverError> {
    let overflow = || ResolverError::InvalidRequest("case sorting sizing overflow".into());
    let count = listing
        .files
        .len()
        .checked_add(listing.directories.len())
        .ok_or_else(overflow)?;
    budget
        .charge(count, 0, 0)
        .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
    let mut bytes = wanted
        .len()
        .checked_mul(8)
        .and_then(|n| n.checked_add(32))
        .ok_or_else(overflow)?;
    for candidate in listing.files.iter().chain(listing.directories.iter()) {
        if cancel.is_cancelled() {
            return Err(ResolverError::Cancelled);
        }
        bytes = bytes
            .checked_add(
                candidate
                    .as_os_str()
                    .len()
                    .checked_mul(8)
                    .and_then(|n| n.checked_add(32))
                    .ok_or_else(overflow)?,
            )
            .ok_or_else(overflow)?;
    }
    Ok(bytes)
}

fn case_result_payload_size(
    paths: &[PathBuf],
    budget: Option<&mut CompileTimeBudget<'_>>,
    cancel: &dyn CancellationToken,
) -> Result<usize, ResolverError> {
    if let Some(budget) = budget {
        budget
            .charge(paths.len(), 0, 0)
            .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
    }
    paths.iter().try_fold(0usize, |sum, path| {
        if cancel.is_cancelled() {
            return Err(ResolverError::Cancelled);
        }
        sum.checked_add(path.as_os_str().len())
            .ok_or_else(|| ResolverError::InvalidRequest("case result path overflow".into()))
    })
}

fn copy_case_paths(
    listing: &DirectoryListing,
    matches: &[usize],
    budget: &mut CompileTimeBudget<'_>,
    cancel: &dyn CancellationToken,
) -> Result<AdmittedOwned<Vec<PathBuf>>, ResolverError> {
    let overflow = || ResolverError::InvalidRequest("case match-set receiving overflow".into());
    let at = |index: usize| {
        if index < listing.files.len() {
            &listing.files[index]
        } else {
            &listing.directories[index - listing.files.len()]
        }
    };
    let backing = matches
        .len()
        .checked_mul(std::mem::size_of::<PathBuf>())
        .ok_or_else(overflow)?;
    budget
        .charge(matches.len(), 0, 0)
        .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
    let payload = matches.iter().try_fold(0usize, |sum, index| {
        if cancel.is_cancelled() {
            return Err(ResolverError::Cancelled);
        }
        sum.checked_add(at(*index).as_os_str().len())
            .ok_or_else(overflow)
    })?;
    budget.admit_fallible(
        matches.len(),
        payload,
        payload.checked_add(backing).ok_or_else(overflow)?,
        crate::compile_time::resolver_error,
        || {
            let mut paths = Vec::with_capacity(matches.len());
            for index in matches {
                if cancel.is_cancelled() {
                    return Err(ResolverError::Cancelled);
                }
                paths.push(at(*index).clone());
            }
            Ok(paths)
        },
    )
}

fn relative_components_owned(
    base: &Path,
    path: &Path,
    budget: &mut CompileTimeBudget<'_>,
    cancel: &dyn CancellationToken,
) -> Result<Option<AdmittedOwned<Vec<std::ffi::OsString>>>, ResolverError> {
    let overflow =
        || ResolverError::InvalidRequest("case relative-component construction overflow".into());
    // Path bytes are O(1) to inspect and bound the component-count traversal.
    // Count scans are separate from the later descriptor construction walks.
    let scan_bound = base
        .as_os_str()
        .len()
        .checked_add(path.as_os_str().len())
        .and_then(|n| n.checked_add(2))
        .ok_or_else(overflow)?;
    budget
        .charge(scan_bound, 0, 0)
        .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
    let count_components = |path: &Path| -> Result<usize, ResolverError> {
        let mut count = 0usize;
        for _ in path.components() {
            if cancel.is_cancelled() {
                return Err(ResolverError::Cancelled);
            }
            count = count.checked_add(1).ok_or_else(overflow)?;
        }
        Ok(count)
    };
    let base_count = count_components(base)?;
    let path_count = count_components(path)?;
    let component_bytes = |count: usize| {
        count
            .checked_mul(std::mem::size_of::<Component<'_>>())
            .ok_or_else(overflow)
    };
    let base_bytes = component_bytes(base_count)?;
    let base_components = budget.admit_fallible(
        base_count,
        0,
        base_bytes,
        crate::compile_time::resolver_error,
        || {
            let mut parts = Vec::with_capacity(base_count);
            for part in base.components() {
                if cancel.is_cancelled() {
                    return Err(ResolverError::Cancelled);
                }
                parts.push(part);
            }
            Ok(parts)
        },
    )?;
    let path_bytes = component_bytes(path_count)?;
    let path_components = budget.admit_fallible(
        path_count,
        0,
        path_bytes,
        crate::compile_time::resolver_error,
        || {
            let mut parts = Vec::with_capacity(path_count);
            for part in path.components() {
                if cancel.is_cancelled() {
                    return Err(ResolverError::Cancelled);
                }
                parts.push(part);
            }
            Ok(parts)
        },
    )?;
    let key_bytes = base
        .as_os_str()
        .len()
        .checked_add(path.as_os_str().len())
        .and_then(|n| n.checked_mul(8))
        .and_then(|n| n.checked_add(32))
        .ok_or_else(overflow)?;
    let _keys = budget
        .admit_owned(base_count, key_bytes, key_bytes, || ())
        .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
    if path_count < base_count {
        return Ok(None);
    }
    let insensitive = path_identity::is_case_insensitive(base);
    for (left, right) in base_components.iter().zip(path_components.iter()) {
        if cancel.is_cancelled() {
            return Err(ResolverError::Cancelled);
        }
        if !path_identity::components_equal(*left, *right, insensitive) {
            return Ok(None);
        }
    }
    let count = path_count - base_count;
    budget
        .charge(count, 0, 0)
        .map_err(|r| crate::compile_time::resolver_error(r, budget))?;
    let payload = path_components[base_count..]
        .iter()
        .try_fold(0usize, |sum, part| {
            if cancel.is_cancelled() {
                return Err(ResolverError::Cancelled);
            }
            match part {
                Component::Normal(name) => sum.checked_add(name.len()).ok_or_else(overflow),
                _ => Ok(sum),
            }
        })?;
    let retained = count
        .checked_mul(std::mem::size_of::<std::ffi::OsString>())
        .and_then(|n| n.checked_add(payload))
        .ok_or_else(overflow)?;
    let relative = budget.admit_fallible(
        count,
        payload,
        retained,
        crate::compile_time::resolver_error,
        || {
            let mut relative = Vec::with_capacity(count);
            for part in &path_components[base_count..] {
                if cancel.is_cancelled() {
                    return Err(ResolverError::Cancelled);
                }
                if let Component::Normal(name) = part {
                    relative.push(name.to_os_string());
                }
            }
            Ok(relative)
        },
    )?;
    Ok(Some(relative))
}

fn relative_components(base: &Path, path: &Path) -> Option<Vec<std::ffi::OsString>> {
    let base_components = base.components().collect::<Vec<_>>();
    let path_components = path.components().collect::<Vec<_>>();
    if path_components.len() < base_components.len()
        || !base_components.iter().zip(path_components.iter()).all(
            |(base_component, path_component)| {
                path_identity::components_equal(
                    *base_component,
                    *path_component,
                    path_identity::is_case_insensitive(base),
                )
            },
        )
    {
        return None;
    }
    Some(
        path_components[base_components.len()..]
            .iter()
            .filter_map(|component| match component {
                Component::Normal(value) => Some(value.to_os_string()),
                _ => None,
            })
            .collect(),
    )
}

fn path_key(path: &Path) -> String {
    path_identity::path_key(&canonical_path(path)).replace('\\', "/")
}

fn path_equivalent(left: &Path, right: &Path) -> bool {
    path_key(left) == path_key(right)
}

fn path_starts_with(path: &Path, root: &Path) -> bool {
    path_identity::path_starts_with(&canonical_path(path), &canonical_path(root))
}

fn context_uses_package_root(context: &ProjectContext, root: &Path) -> bool {
    context
        .main_source_entry
        .iter()
        .chain(context.search_path_entries.iter())
        .chain(context.include_path_entries.iter())
        .chain(context.explicit_unit_entries.values().flatten())
        .any(|entry| match &entry.provenance {
            ProjectPathProvenance::Mapped { root: mapped_root } => {
                path_equivalent(mapped_root, root) || path_starts_with(&entry.path, root)
            }
            ProjectPathProvenance::Configured => path_starts_with(&entry.path, root),
            ProjectPathProvenance::LegacyNative => false,
        })
}

fn resolved_package_root(context: &ProjectContext, configured_root: &Path) -> PathBuf {
    let configured_root = canonical_path(configured_root);
    context
        .main_source_entry
        .iter()
        .chain(context.search_path_entries.iter())
        .chain(context.include_path_entries.iter())
        .chain(context.explicit_unit_entries.values().flatten())
        .find_map(|entry| match &entry.provenance {
            ProjectPathProvenance::Mapped { root }
                if path_equivalent_ignore_case(root, &configured_root) =>
            {
                Some(canonical_path(root))
            }
            _ => None,
        })
        .unwrap_or(configured_root)
}

fn path_equivalent_ignore_case(left: &Path, right: &Path) -> bool {
    let left = canonical_path(left);
    let right = canonical_path(right);
    let left_components = left.components().collect::<Vec<_>>();
    let right_components = right.components().collect::<Vec<_>>();
    left_components.len() == right_components.len()
        && left_components
            .iter()
            .zip(right_components.iter())
            .all(|(left, right)| {
                left.as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
            })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admission_root(text: &str) -> ResolvedUnit {
        ResolvedUnit {
            requested_name: "T".into(),
            declared_name: "T".into(),
            source: LoadedSource {
                id: SourceId::new("source:/workspace/T.pas"),
                path: PathBuf::from("/workspace/T.pas"),
                bytes: Arc::from(text.as_bytes()),
                decoded_text: None,
                compile_time_backing: None,
                revision: SourceRevision::Overlay {
                    version: 1,
                    content_hash: 0,
                },
            },
        }
    }

    fn admission_resolver(limits: ResolverLimits) -> UnitResolver<MemoryStore> {
        UnitResolver::new(
            ProjectContext {
                discovery_complete: true,
                search_paths: vec![PathBuf::from("/workspace")],
                search_path_entries: vec![ProjectPathEntry::legacy(PathBuf::from("/workspace"))],
                ..Default::default()
            },
            vec![PathBuf::from("/workspace")],
            MemoryStore::default(),
            limits,
        )
    }

    fn root_output_reservation(root: &ResolvedUnit) -> usize {
        loaded_source_retained(&root.source)
            + root.requested_name.len()
            + root.declared_name.len()
            + std::mem::size_of::<ResolvedUnit>()
    }

    fn root_context_output_reservation(
        resolver: &UnitResolver<MemoryStore>,
        root: &ResolvedUnit,
    ) -> usize {
        std::mem::size_of::<(SourceId, pascal_project::ConditionalContext)>()
            + root.source.id.as_str().len()
            + conditional_context_reserve(&resolver.context).unwrap()
    }

    #[test]
    fn fix2_queued_sites_aggregate_capacity_is_admitted_before_growth() {
        let root = admission_root("unit T; interface implementation end.");
        let sites = vec![ImportSite {
            requested_name: "Provider".repeat(8),
            byte_range: 0..1,
            section: ImportSection::Interface,
        }];
        let site_bytes = std::mem::size_of::<ImportSite>() + sites[0].requested_name.len();
        let mut r = admission_resolver(ResolverLimits {
            max_compile_time_retained_bytes: 2 * site_bytes
                + 3 * std::mem::size_of::<QueuedProjectImporter>(),
            ..Default::default()
        });
        let mut queue = ProjectImportQueue::default();
        for _ in 0..2 {
            r.enqueue_project_imports(&mut queue, &root, &sites, &NoCancellation)
                .unwrap();
        }
        assert_eq!(queue.site_retained, 2 * site_bytes);
        assert_eq!(
            queue.storage_retained,
            queue.items.capacity() * std::mem::size_of::<QueuedProjectImporter>()
        );
        assert_eq!(
            r.compile_time_usage().retained_bytes,
            queue.site_retained + queue.storage_retained
        );
        let capacity = queue.items.capacity();
        let before = r.compile_time_usage();
        assert!(matches!(
            r.enqueue_project_imports(&mut queue, &root, &sites, &NoCancellation),
            Err(ResolverError::LimitExceeded { .. })
        ));
        assert_eq!(queue.items.len(), 2);
        assert_eq!(queue.items.capacity(), capacity);
        assert_eq!(r.compile_time_usage().retained_bytes, before.retained_bytes);
        assert!(r.compile_time_usage().work > before.work);
        assert!(r.report().observations.is_empty());
    }

    #[test]
    fn fix2_parsed_root_queue_sites_and_capacity_refund_on_consumption() {
        let root =
            admission_root("unit T; interface {$IF false}uses A,B,C;{$ENDIF} implementation end.");
        let initial = root_output_reservation(&root);
        let mut r = admission_resolver(Default::default());
        let context_output = root_context_output_reservation(&r, &root);
        let p = r.resolve_project_inner(root, &[], &NoCancellation).unwrap();
        assert!(p.complete);
        assert_eq!(p.conditional_contexts.len(), 1);
        assert_eq!(
            p.conditional_contexts[&p.root.source.id],
            r.context.conditional_context_for(&p.root.source.path)
        );
        assert_eq!(p.imports.len(), 3);
        assert!(p.report.observations.is_empty());
        let output = initial
            + context_output
            + p.imports
                .iter()
                .map(|i| {
                    std::mem::size_of::<ResolvedImport>()
                    + i.site.requested_name.len()
                    + i.importer_source_id.as_str().len()
                    // Existing conservative qualifier-output reservation, even
                    // for unavailable bindings (not queue input storage).
                    + 8 * std::mem::size_of::<String>()
                    + 8 * i.site.requested_name.len()
                })
                .sum::<usize>();
        assert_eq!(
            r.compile_time_usage().retained_bytes,
            output,
            "only existing published output reservations survive, not queue sites/capacity"
        );
        assert!(r.compile_time_usage().work > 3);
    }

    #[test]
    fn fix2_popped_site_and_queue_refunds_on_error_cancel_and_abandonment() {
        struct CancelAfterAdmission(std::cell::Cell<usize>);
        impl CancellationToken for CancelAfterAdmission {
            fn is_cancelled(&self) -> bool {
                let n = self.0.get();
                self.0.set(n + 1);
                n >= 4
            }
        }
        let root = admission_root("unit T; interface implementation end.");
        let initial = root_output_reservation(&root);
        let sites = [ImportSite {
            requested_name: "ForbiddenProvider".into(),
            byte_range: 0..1,
            section: ImportSection::Interface,
        }];
        let mut r = admission_resolver(ResolverLimits {
            max_compile_time_work: 50,
            ..Default::default()
        });
        assert!(matches!(
            r.resolve_project_inner(root.clone(), &sites, &NoCancellation),
            Err(ResolverError::LimitExceeded { .. })
        ));
        assert_eq!(r.compile_time_usage().retained_bytes, initial);
        assert!(r.compile_time_usage().work > 0);
        assert!(r.report().observations.is_empty());
        let mut r = admission_resolver(Default::default());
        assert_eq!(
            r.resolve_project_inner(
                root.clone(),
                &sites,
                &CancelAfterAdmission(std::cell::Cell::new(0))
            )
            .unwrap_err(),
            ResolverError::Cancelled
        );
        assert_eq!(r.compile_time_usage().retained_bytes, initial);
        assert!(r.compile_time_usage().work > 0);
        assert!(r.report().observations.is_empty());
        let mut r = admission_resolver(Default::default());
        r.stop_project_walk_when_incomplete(true);
        r.mark_incomplete("prior lookup failure".into());
        let context_output = root_context_output_reservation(&r, &root);
        let p = r
            .resolve_project_inner(root, &sites, &NoCancellation)
            .unwrap();
        assert!(!p.complete);
        assert!(p.imports.is_empty());
        assert_eq!(p.conditional_contexts.len(), 1);
        assert_eq!(
            p.conditional_contexts[&p.root.source.id],
            r.context.conditional_context_for(&p.root.source.path)
        );
        assert_eq!(
            r.compile_time_usage().retained_bytes,
            initial + context_output
        );
        assert!(r.compile_time_usage().work > 0);
        assert_eq!(p.report.incomplete_reasons, vec!["prior lookup failure"]);
    }

    #[test]
    fn fix2_parsed_dependency_queue_is_consumed_without_retaining_site_copies() {
        let root = admission_root("unit T; interface uses A,B; implementation end.");
        let mut r = admission_resolver(Default::default());
        for name in ["A", "B"] {
            r.store.sources.insert(
                PathBuf::from(format!("/workspace/{name}.pas")),
                format!(
                    "unit {name}; interface {{$IF false}}uses Unread;{{$ENDIF}} implementation end."
                )
                .into_bytes(),
            );
        }
        let mut queue = ProjectImportQueue::default();
        let p = r
            .resolve_project_queued(root, &[], &NoCancellation, &mut queue)
            .unwrap();
        assert!(p.complete);
        assert_eq!(p.units.len(), 2);
        assert_eq!(p.imports.len(), 4);
        assert!(queue.items.is_empty());
        assert_eq!(
            queue.site_retained, 0,
            "root and both parsed dependency inputs were consumed/refunded"
        );
        assert_eq!(
            queue.storage_retained,
            queue.items.capacity() * std::mem::size_of::<QueuedProjectImporter>()
        );
        assert!(queue.storage_retained > 0);
        assert_eq!(
            p.report
                .observations
                .iter()
                .filter_map(|o| match o {
                    ResolutionObservation::Payload { path, .. } => Some(path.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec![
                PathBuf::from("/workspace/A.pas"),
                PathBuf::from("/workspace/B.pas")
            ]
        );
    }

    #[test]
    fn fix2_cancelled_dependency_walk_refunds_unconsumed_queue_tail_and_preserves_reads() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct Flag(Arc<AtomicBool>);
        impl CancellationToken for Flag {
            fn is_cancelled(&self) -> bool {
                self.0.load(Ordering::Relaxed)
            }
        }
        struct CancelInclude {
            inner: MemoryStore,
            flag: Arc<AtomicBool>,
        }
        impl SourceStore for CancelInclude {
            fn list_directory_admitted(
                &mut self,
                request: DirectoryRequest<'_>,
                budget: &mut crate::compile_time::CompileTimeBudget<'_>,
                cancel: &dyn CancellationToken,
            ) -> Result<crate::compile_time::AdmittedOwned<DirectoryListing>, SourceStoreError>
            {
                // This fixture's entire directory is known before construction.
                // Admit the copied paths and receiving vector, not a legacy
                // listing obtained first and measured after allocation.
                let count = self
                    .inner
                    .sources
                    .keys()
                    .filter(|path| path.parent() == Some(request.directory))
                    .count();
                let paths = self
                    .inner
                    .sources
                    .keys()
                    .filter(|path| path.parent() == Some(request.directory))
                    .map(|path| path.as_os_str().len())
                    .sum::<usize>();
                let payload = paths + count * std::mem::size_of::<PathBuf>();
                budget.admit_fallible(1, paths, payload, store_admission_error, || {
                    if cancel.is_cancelled() {
                        return Err(SourceStoreError::Cancelled);
                    }
                    let mut files = Vec::with_capacity(count);
                    files.extend(
                        self.inner
                            .sources
                            .keys()
                            .filter(|path| path.parent() == Some(request.directory))
                            .cloned(),
                    );
                    Ok(DirectoryListing {
                        files,
                        directories: Vec::new(),
                        stamp: None,
                        complete: true,
                    })
                })
            }
            fn overlay_candidates_admitted(
                &self,
                _roots: &[PathBuf],
                _names: &[String],
                budget: &mut crate::compile_time::CompileTimeBudget<'_>,
                _cancel: &dyn CancellationToken,
            ) -> Result<crate::compile_time::AdmittedOwned<Vec<PathBuf>>, SourceStoreError>
            {
                // MemoryStore has no overlay candidates.
                budget
                    .admit_owned(1, 0, 0, Vec::new)
                    .map_err(|reason| store_admission_error(reason, budget))
            }
            fn load_admitted(
                &mut self,
                request: SourceRequest<'_>,
                budget: &mut crate::compile_time::CompileTimeBudget<'_>,
                cancel: &dyn CancellationToken,
            ) -> Result<LoadedSource, SourceStoreError> {
                let bytes = self.inner.sources.get(request.path).map_or(0, Vec::len);
                let metadata = 2 * request.path.as_os_str().len() + "source:".len();
                // Legacy MemoryStore copies its Vec into an Arc. Both buffers
                // coexist during that bounded conversion; include the peak and
                // formatting capacity before forwarding to its constructor.
                LoadedSource::admit_construction(budget, bytes, 2 * (bytes + metadata), || {
                    self.load(request, cancel)
                })
            }
            fn list_directory(
                &mut self,
                request: DirectoryRequest<'_>,
                cancel: &dyn CancellationToken,
            ) -> Result<DirectoryListing, SourceStoreError> {
                self.inner.list_directory(request, cancel)
            }
            fn overlay_candidates(&self, roots: &[PathBuf], names: &[String]) -> Vec<PathBuf> {
                self.inner.overlay_candidates(roots, names)
            }
            fn load(
                &mut self,
                request: SourceRequest<'_>,
                cancel: &dyn CancellationToken,
            ) -> Result<LoadedSource, SourceStoreError> {
                if request.path.file_name().is_some_and(|n| n == "halt.inc") {
                    self.flag.store(true, Ordering::Relaxed);
                    Err(SourceStoreError::Cancelled)
                } else {
                    self.inner.load(request, cancel)
                }
            }
        }
        let new_resolver = || {
            let flag = Arc::new(AtomicBool::new(false));
            let mut inner = MemoryStore::default();
            for (name, text) in [
                (
                    "A.pas",
                    "unit A; interface {$I halt.inc}uses NeverA; implementation end.",
                ),
                (
                    "B.pas",
                    "unit B; interface uses NeverB; implementation end.",
                ),
                (
                    "C.pas",
                    "unit C; interface uses NeverC; implementation end.",
                ),
                ("halt.inc", "{never returned}"),
            ] {
                inner.sources.insert(
                    PathBuf::from(format!("/workspace/{name}")),
                    text.as_bytes().to_vec(),
                );
            }
            (
                UnitResolver::new(
                    admission_resolver(Default::default()).context,
                    vec![PathBuf::from("/workspace")],
                    CancelInclude {
                        inner,
                        flag: flag.clone(),
                    },
                    Default::default(),
                ),
                Flag(flag),
            )
        };
        let root = admission_root("unit T; interface uses A,B,C; implementation end.");
        let (mut partial, cancel) = new_resolver();
        let mut queue = ProjectImportQueue::default();
        assert_eq!(
            partial
                .resolve_project_queued(root.clone(), &[], &cancel, &mut queue)
                .unwrap_err(),
            ResolverError::Cancelled
        );
        assert_eq!(queue.items.len(), 2);
        let tail = queue
            .items
            .iter()
            .map(|item| {
                item.sites.capacity() * std::mem::size_of::<ImportSite>()
                    + item
                        .sites
                        .iter()
                        .map(|s| s.requested_name.capacity())
                        .sum::<usize>()
            })
            .sum::<usize>();
        assert_eq!(
            queue.site_retained, tail,
            "popped A sites refunded; B/C remain until queue owner drops them"
        );
        let temporary = queue.site_retained + queue.storage_retained;
        let (mut owned, cancel) = new_resolver();
        assert_eq!(
            owned.resolve_project_inner(root, &[], &cancel).unwrap_err(),
            ResolverError::Cancelled
        );
        assert_eq!(
            owned.compile_time_usage().retained_bytes + temporary,
            partial.compile_time_usage().retained_bytes
        );
        assert_eq!(
            owned.compile_time_usage().work,
            partial.compile_time_usage().work,
            "temporary cleanup must not refund work"
        );
        let payloads = owned
            .report()
            .observations
            .iter()
            .filter_map(|o| match o {
                ResolutionObservation::Payload { path, .. } => Some(path.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            payloads,
            vec![
                PathBuf::from("/workspace/A.pas"),
                PathBuf::from("/workspace/B.pas"),
                PathBuf::from("/workspace/C.pas")
            ]
        );
        assert!(!owned.report().complete);
        assert_eq!(
            owned.report().observations,
            partial.report().observations,
            "failure cleanup preserves lookup evidence"
        );
    }

    fn physical_report_resolver() -> (
        tempfile::TempDir,
        PathBuf,
        UnitResolver<FilesystemSourceStore>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Main.pas");
        fs::write(&root, "unit Main; interface implementation end.").unwrap();
        fs::write(dir.path().join("value.inc"), "{physical include}").unwrap();
        fs::write(
            dir.path().join("Other.pas"),
            "unit Other; interface implementation end.",
        )
        .unwrap();
        let context =
            ProjectContext::discover(&root, &[dir.path().into()], &Default::default()).unwrap();
        let resolver = UnitResolver::new(
            context,
            vec![dir.path().into()],
            FilesystemSourceStore::new(),
            Default::default(),
        );
        (dir, root, resolver)
    }

    #[test]
    fn case_match_owner_actual_found_refunds_only_returned_path_after_helper_exit() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        let (dir, root, mut resolver) = physical_report_resolver();
        let entry = resolver.context.path_entry_for(dir.path()).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let source = resolver
            .include_capability(
                IncludeResolveRequest {
                    including_path: &root,
                    byte_range: 0..0,
                    requested_name: "value.inc",
                    legacy_route: None,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
        let mut call = resolver.store_call(&mut budget).unwrap();
        drop(
            call.list_directory(dir.path(), &entry, None, &NoCancellation, false)
                .unwrap(),
        );
        let before = budget.snapshot();
        let result = call
            .resolve_case_insensitive_path(
                dir.path(),
                &dir.path().join("VaLuE.InC"),
                &entry,
                None,
                &NoCancellation,
            )
            .unwrap();
        let CaseInsensitiveLookup::Found(path) = result else {
            panic!("physical case match")
        };
        assert_eq!(path.as_os_str(), dir.path().join("value.inc").as_os_str());
        let invoice = std::mem::size_of::<AdmittedOwned<PathBuf>>() + path.as_os_str().len();
        assert_eq!(
            budget.snapshot().retained_bytes,
            before.retained_bytes + invoice,
            "only the returned matched path survives helper scratch exit"
        );
        let spent = budget.snapshot();
        drop(path);
        assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
        assert_eq!(budget.snapshot().work, spent.work);
        assert_eq!(budget.snapshot().byte_work, spent.byte_work);
        assert!(matches!(source.result(), Resolution::Found(_)));
        assert!(!source.reports().observations().is_empty());
    }

    #[test]
    fn case_match_owner_actual_nested_include_unit_and_exact_preference() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        let (dir, root, mut resolver) = physical_report_resolver();
        let folder = dir.path().join("MiXeD");
        let sub = folder.join("SuB");
        fs::create_dir_all(&sub).unwrap();
        let body = sub.join("body.inc");
        fs::write(&body, "{nested physical include}").unwrap();
        let provider = folder.join("THING.PAS");
        fs::write(
            &provider,
            "unit Mixed.Thing; interface type T=Byte; implementation end.",
        )
        .unwrap();
        let entry = resolver.context.path_entry_for(dir.path()).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let include = resolver
            .include_capability(
                IncludeResolveRequest {
                    including_path: &root,
                    byte_range: 0..0,
                    requested_name: "mixed/sub/BODY.inc",
                    legacy_route: None,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
        assert!(
            matches!(include.result(),Resolution::Found(source) if source.path==body && source.compile_time_backing.is_some()),
            "actual include result: {:?}; warnings: {:?}",
            include.result(),
            include.reports().warnings()
        );
        assert!(include.reports().observations().iter().any(|o|matches!(o,ResolutionObservation::Payload { path,revision:SourceRevision::Disk { path_entry:e,.. },.. } if path==&body && e.provenance==entry.provenance)));
        let unit = resolver
            .unit_report_capability(
                UnitResolveRequest {
                    importer_path: &root,
                    requested_name: "mixed.Thing",
                    legacy_route: None,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
        assert!(
            matches!(unit.result(),Resolution::Found(source) if source.unit.source.path==provider && source.unit.declared_name.eq_ignore_ascii_case("Mixed.Thing"))
        );
        assert!(!unit.reports().observations().is_empty());
        let mut call = resolver.store_call(&mut budget).unwrap();
        let result = call
            .resolve_case_insensitive_path(dir.path(), &body, &entry, None, &NoCancellation)
            .unwrap();
        assert!(matches!(&result,CaseInsensitiveLookup::Found(path) if **path==body));
        drop(result);
        if !path_identity::is_case_insensitive(dir.path()) {
            fs::write(dir.path().join("OTHER.pas"), "unit Other;").unwrap();
            let exact = call
                .resolve_case_insensitive_path(
                    dir.path(),
                    &dir.path().join("Other.pas"),
                    &entry,
                    None,
                    &NoCancellation,
                )
                .unwrap();
            assert!(
                matches!(&exact,CaseInsensitiveLookup::Found(path) if **path==dir.path().join("Other.pas"))
            );
            let ambiguous = call
                .resolve_case_insensitive_path(
                    dir.path(),
                    &dir.path().join("oThEr.pas"),
                    &entry,
                    None,
                    &NoCancellation,
                )
                .unwrap();
            assert!(matches!(&ambiguous,CaseInsensitiveLookup::Ambiguous(paths) if paths.len()==2));
        }
    }

    #[test]
    fn case_match_owner_returned_found_and_ambiguity_survive_cache_and_budget_last_drop() {
        use crate::compile_time::CompileTimeLimits;
        for ambiguous in [false, true] {
            let (dir, _, mut resolver) = physical_report_resolver();
            if ambiguous {
                if path_identity::is_case_insensitive(dir.path()) {
                    continue;
                }
                fs::write(dir.path().join("VALUE.inc"), "{other physical file}").unwrap();
            }
            let entry = resolver.context.path_entry_for(dir.path()).unwrap();
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let observer = budget
                .admit_owned(0, 0, 0, || ())
                .unwrap()
                .into_shared(&mut budget)
                .unwrap();
            let baseline = budget.snapshot().retained_bytes;
            let result;
            let invoice;
            {
                let mut call = resolver.store_call(&mut budget).unwrap();
                drop(
                    call.list_directory(dir.path(), &entry, None, &NoCancellation, false)
                        .unwrap(),
                );
                let before = budget.snapshot();
                result = call
                    .resolve_case_insensitive_path(
                        dir.path(),
                        &dir.path().join("VaLuE.InC"),
                        &entry,
                        None,
                        &NoCancellation,
                    )
                    .unwrap();
                invoice = match &result {
                    CaseInsensitiveLookup::Found(path) => {
                        std::mem::size_of::<AdmittedOwned<PathBuf>>() + path.as_os_str().len()
                    }
                    CaseInsensitiveLookup::Ambiguous(paths) => {
                        std::mem::size_of::<AdmittedOwned<Vec<PathBuf>>>()
                            + paths.capacity() * std::mem::size_of::<PathBuf>()
                            + paths.iter().map(|p| p.as_os_str().len()).sum::<usize>()
                    }
                    _ => panic!("expected physical matched owner"),
                };
                assert_eq!(
                    budget.snapshot().retained_bytes,
                    before.retained_bytes + invoice,
                    "only the matched return survives, not unrelated helper storage"
                );
            }
            drop(resolver);
            drop(budget);
            let live = observer.budget_snapshot();
            assert_eq!(live.retained_bytes, baseline + invoice);
            match &result {
                CaseInsensitiveLookup::Found(path) => assert!(path.ends_with("value.inc")),
                CaseInsensitiveLookup::Ambiguous(paths) => assert_eq!(paths.len(), 2),
                _ => unreachable!(),
            }
            drop(result);
            assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
            assert_eq!(observer.budget_snapshot().work, live.work);
            assert_eq!(observer.budget_snapshot().byte_work, live.byte_work);
        }
    }

    #[test]
    fn case_match_owner_missing_overlay_dedup_and_authorization_preserve_live_directory() {
        use crate::compile_time::CompileTimeLimits;
        let (dir, _, mut resolver) = directory_probe_resolver();
        let overlay = dir.path().join("value.inc");
        resolver.store.inner.insert_overlay(
            &overlay,
            1,
            Arc::<[u8]>::from(&b"{overlay include}"[..]),
        );
        let entry = resolver.context.path_entry_for(dir.path()).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut call = resolver.store_call(&mut budget).unwrap();
        let directory = call
            .list_directory(dir.path(), &entry, None, &NoCancellation, false)
            .unwrap();
        let directory_pointer = directory.files.as_ptr();
        let result = call
            .resolve_case_insensitive_path(
                dir.path(),
                &dir.path().join("VaLuE.InC"),
                &entry,
                None,
                &NoCancellation,
            )
            .unwrap();
        assert!(
            matches!(&result,CaseInsensitiveLookup::Found(path) if **path==overlay),
            "same physical disk+overlay listing is not an ambiguity"
        );
        drop(result);
        assert_eq!(directory.files.as_ptr(), directory_pointer);
        assert!(directory.files.contains(&overlay));
        // Once the original caller is dropped, reread replacement has the same
        // cached backing cost; Missing must retain no helper construction.
        drop(directory);
        let before = budget.snapshot();
        let result = call
            .resolve_case_insensitive_path(
                dir.path(),
                &dir.path().join("absent.inc"),
                &entry,
                None,
                &NoCancellation,
            )
            .unwrap();
        assert!(matches!(result, CaseInsensitiveLookup::Missing));
        assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
        let outside = dir.path().parent().unwrap().join("not-this-base.inc");
        assert!(matches!(
            call.resolve_case_insensitive_path(dir.path(), &outside, &entry, None, &NoCancellation)
                .unwrap(),
            CaseInsensitiveLookup::Missing
        ));
        assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
        let denied = ProjectPathEntry {
            path: entry.path.clone(),
            provenance: ProjectPathProvenance::Mapped {
                root: dir.path().join("not-authorized"),
            },
        };
        let calls = call.store.calls;
        assert!(matches!(
            call.resolve_case_insensitive_path(
                dir.path(),
                &overlay,
                &denied,
                None,
                &NoCancellation
            ),
            Err(ResolverError::SourceStore(
                SourceStoreError::AdmissionUnavailable
            ))
        ));
        assert!(call.store.calls > calls);
        assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
    }

    #[test]
    fn case_match_owner_unit_ambiguous_component_builds_independent_plain_result_copy() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        let (dir, root, mut resolver) = physical_report_resolver();
        if path_identity::is_case_insensitive(dir.path()) {
            return;
        }
        for name in ["Space", "SPACE"] {
            fs::create_dir(dir.path().join(name)).unwrap();
        }
        let entry = resolver.context.path_entry_for(dir.path()).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let (first, second);
        {
            let mut call = resolver.store_call(&mut budget).unwrap();
            let matches = call
                .resolve_case_insensitive_path(
                    dir.path(),
                    &dir.path().join("sPaCe"),
                    &entry,
                    None,
                    &NoCancellation,
                )
                .unwrap();
            let CaseInsensitiveLookup::Ambiguous(paths) = matches else {
                panic!("two physical component matches")
            };
            first = paths[0].as_os_str().as_encoded_bytes().as_ptr();
            second = paths[1].as_os_str().as_encoded_bytes().as_ptr();
            // Keep original owner through the receiving construction to prove
            // these are independently admitted/copy-owned result path buffers.
            let out = call
                .unit_report_capability(
                    UnitResolveRequest {
                        importer_path: &root,
                        requested_name: "sPaCe.Other",
                        legacy_route: None,
                    },
                    &mut budget,
                    &NoCancellation,
                )
                .unwrap();
            assert!(
                matches!(out.result(),Resolution::Ambiguous { candidates } if candidates.len()==2 && candidates.iter().all(|c|c.declared_name.is_none() && c.path.as_os_str().as_encoded_bytes().as_ptr()!=first && c.path.as_os_str().as_encoded_bytes().as_ptr()!=second))
            );
            drop(paths);
            assert!(
                matches!(out.result(),Resolution::Ambiguous { candidates } if candidates.iter().all(|c|c.path.is_dir()))
            );
            let spent = budget.snapshot();
            drop(out);
            assert_eq!(budget.snapshot().work, spent.work);
            assert_eq!(budget.snapshot().byte_work, spent.byte_work);
        }
    }

    #[test]
    fn case_match_owner_nested_callback_error_cancel_unwind_and_partial_polls_retire_scratch() {
        use crate::compile_time::CompileTimeLimits;
        for failure in [
            DirectoryFailure::Error,
            DirectoryFailure::Cancel,
            DirectoryFailure::Panic,
            DirectoryFailure::Foreign,
        ] {
            let (dir, _, mut resolver) = directory_probe_resolver();
            let nested = dir.path().join("Nested");
            fs::create_dir(&nested).unwrap();
            fs::write(nested.join("Leaf.inc"), "{leaf}").unwrap();
            let entry = resolver.context.path_entry_for(dir.path()).unwrap();
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let observer = budget
                .admit_owned(0, 0, 0, || ())
                .unwrap()
                .into_shared(&mut budget)
                .unwrap();
            let baseline = budget.snapshot().retained_bytes;
            let mut call = resolver.store_call(&mut budget).unwrap();
            call.store.failure = failure;
            call.store.failure_call = Some(2);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                call.resolve_case_insensitive_path(
                    dir.path(),
                    &dir.path().join("nested/LEAF.inc"),
                    &entry,
                    None,
                    &NoCancellation,
                )
            }));
            assert_eq!(
                call.store.calls, 2,
                "reach actual second component callback with previous selected path live"
            );
            if matches!(failure, DirectoryFailure::Panic) {
                assert!(result.is_err());
            } else {
                assert!(result.unwrap().is_err());
            }
            drop(call);
            drop(resolver);
            assert_eq!(
                budget.snapshot().retained_bytes,
                baseline,
                "failed nested lookup leaves no leaked temporary construction"
            );
            assert!(budget.snapshot().work > 0);
            assert!(budget.snapshot().byte_work > 0);
            assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
        }
        struct Poll {
            count: std::sync::atomic::AtomicUsize,
            stop: usize,
        }
        impl CancellationToken for Poll {
            fn is_cancelled(&self) -> bool {
                self.count.fetch_add(1, Ordering::Relaxed) >= self.stop
            }
        }
        let mut cancelled = 0;
        let mut complete = false;
        for stop in 0..400 {
            let (dir, _, mut resolver) = directory_probe_resolver();
            let nested = dir.path().join("Nested");
            fs::create_dir(&nested).unwrap();
            fs::write(nested.join("Leaf.inc"), "{leaf}").unwrap();
            let entry = resolver.context.path_entry_for(dir.path()).unwrap();
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let observer = budget
                .admit_owned(0, 0, 0, || ())
                .unwrap()
                .into_shared(&mut budget)
                .unwrap();
            let baseline = budget.snapshot().retained_bytes;
            let result = {
                let mut call = resolver.store_call(&mut budget).unwrap();
                call.resolve_case_insensitive_path(
                    dir.path(),
                    &dir.path().join("nested/LEAF.inc"),
                    &entry,
                    None,
                    &Poll {
                        count: 0.into(),
                        stop,
                    },
                )
            };
            match result {
                Err(ResolverError::Cancelled) => cancelled += 1,
                Ok(CaseInsensitiveLookup::Found(path)) => {
                    assert!(path.ends_with("Nested/Leaf.inc"));
                    drop(path);
                    complete = true;
                }
                Err(other) => panic!("unexpected stop {stop}: {other:?}"),
                _ => panic!("unexpected match at stop {stop}"),
            }
            let spent = budget.snapshot();
            drop(resolver);
            assert_eq!(
                budget.snapshot().retained_bytes,
                baseline,
                "partial helper allocation at poll {stop}"
            );
            assert_eq!(budget.snapshot().work, spent.work);
            assert_eq!(budget.snapshot().byte_work, spent.byte_work);
            assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
            if complete {
                break;
            }
        }
        assert!(complete);
        assert!(
            cancelled > 20,
            "exercise deep callbacks, partial components, matching and returned copying"
        );
    }

    #[test]
    fn case_match_owner_precanonical_component_and_receiving_copy_refusals_keep_live_owners() {
        use crate::compile_time::CompileTimeLimits;
        let (dir, _, mut resolver) = directory_probe_resolver();
        let entry = resolver.context.path_entry_for(dir.path()).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut call = resolver.store_call(&mut budget).unwrap();
        let listing = call
            .list_directory(dir.path(), &entry, None, &NoCancellation, false)
            .unwrap();
        let pointer = listing.files.as_ptr();
        let before = budget.snapshot();
        budget.constrain_retained(before.retained_bytes).unwrap();
        let calls = call.store.calls;
        drop(call);
        let mut call = resolver.store_call(&mut budget).unwrap();
        assert!(matches!(
            call.resolve_case_insensitive_path(
                dir.path(),
                &dir.path().join("VaLuE.inc"),
                &entry,
                None,
                &NoCancellation
            ),
            Err(ResolverError::LimitExceeded { .. })
        ));
        assert_eq!(
            call.store.calls, calls,
            "canonical construction refusal precedes the directory callback"
        );
        assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
        assert!(matches!(
            relative_components_owned(
                dir.path(),
                &dir.path().join("value.inc"),
                &mut budget,
                &NoCancellation
            ),
            Err(ResolverError::LimitExceeded { .. })
        ));
        assert!(matches!(
            copy_case_path(&listing.files[0], &mut budget),
            Err(ResolverError::LimitExceeded { .. })
        ));
        assert!(matches!(
            copy_case_paths(&listing, &[0], &mut budget, &NoCancellation),
            Err(ResolverError::LimitExceeded { .. })
        ));
        assert_eq!(listing.files.as_ptr(), pointer);
        assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
        let sizing_work =
            dir.path().as_os_str().len() + dir.path().join("value.inc").as_os_str().len() + 2 + 1;
        assert_eq!(
            budget.snapshot().work,
            before.work + sizing_work,
            "component counting and match payload sizing remain spent on later retention refusal"
        );
        assert_eq!(budget.snapshot().byte_work, before.byte_work);
    }

    #[test]
    fn case_match_f1_payload_sizing_retained_refusal_spends_scan_work() {
        use crate::compile_time::CompileTimeLimits;
        let (dir, _, mut resolver) = directory_probe_resolver();
        let entry = resolver.context.path_entry_for(dir.path()).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut call = resolver.store_call(&mut budget).unwrap();
        let listing = call
            .list_directory(dir.path(), &entry, None, &NoCancellation, false)
            .unwrap();
        let before = budget.snapshot();
        budget.constrain_retained(before.retained_bytes).unwrap();
        assert!(matches!(
            copy_case_paths(&listing, &[0], &mut budget, &NoCancellation),
            Err(ResolverError::LimitExceeded { .. })
        ));
        assert_eq!(
            budget.snapshot().work,
            before.work + 1,
            "payload sizing is spent before receiving retention refusal"
        );
        assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
        assert!(!listing.files.is_empty());
    }

    #[test]
    fn case_match_f1_component_count_polls_before_retained_refusal() {
        use crate::compile_time::CompileTimeLimits;
        struct Cancel;
        impl CancellationToken for Cancel {
            fn is_cancelled(&self) -> bool {
                true
            }
        }
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        budget.constrain_retained(0).unwrap();
        assert!(
            matches!(
                relative_components_owned(
                    Path::new("/one"),
                    Path::new("/one/two/three"),
                    &mut budget,
                    &Cancel
                ),
                Err(ResolverError::Cancelled)
            ),
            "cancellation must interrupt the admitted count scan before construction"
        );
        assert!(budget.snapshot().work > 0);
        assert_eq!(budget.snapshot().retained_bytes, 0);
    }

    #[test]
    fn escaped_result_owner_physical_include_reason_survives_report_cache_and_budget_drop() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        for name in ["not-present.inc", ""] {
            let (_dir, root, mut resolver) = physical_report_resolver();
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let observer = budget
                .admit_owned(0, 0, 0, || ())
                .unwrap()
                .into_shared(&mut budget)
                .unwrap();
            let baseline = budget.snapshot().retained_bytes;
            let outcome = resolver
                .include_capability(
                    IncludeResolveRequest {
                        including_path: &root,
                        byte_range: 0..0,
                        requested_name: name,
                        legacy_route: None,
                    },
                    &mut budget,
                    &NoCancellation,
                )
                .unwrap();
            let pointer = match outcome.result() {
                Resolution::Unavailable { reason } | Resolution::Incomplete { reason, .. } => {
                    reason.as_ptr()
                }
                other => panic!("expected actual missing/incomplete result, got {other:?}"),
            };
            let (result, reports) = outcome.into_parts();
            drop(reports);
            drop(resolver);
            drop(budget);
            let live = observer.budget_snapshot();
            let reason = match result.result() {
                Resolution::Unavailable { reason } | Resolution::Incomplete { reason, .. } => {
                    reason
                }
                _ => unreachable!(),
            };
            assert_eq!(
                reason.as_ptr(),
                pointer,
                "owning callback consumption must not clone the already constructed result reason"
            );
            assert!(
                live.retained_bytes >= baseline + reason.len(),
                "live escaped reason requires independent original-construction credit after unrelated owners die"
            );
            assert_eq!(
                live.retained_bytes,
                baseline
                    + reason.capacity()
                    + std::mem::size_of::<AdmittedOwned<Resolution<LoadedSource>>>(),
                "only the original reason construction and its current inline carrier survive"
            );
            drop(result);
            assert_eq!(
                observer.budget_snapshot().retained_bytes,
                baseline,
                "last result drop retires its metadata, not a request-lifetime floor"
            );
            assert_eq!(observer.budget_snapshot().work, live.work);
            assert_eq!(observer.budget_snapshot().byte_work, live.byte_work);
        }
    }

    #[test]
    fn found_unit_name_owner_actual_provider_promotion_preserves_selected_pointers() {
        use crate::compile_time::{CompileTimeLimits, ProviderSource};
        let (_dir, root, mut resolver) = physical_report_resolver();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut callback = resolver.callback_report_call();
        callback.owning_unit_callback = true;
        let outcome = callback
            .store_call(&mut budget)
            .unwrap()
            .try_resolve_unit(
                UnitResolveRequest {
                    importer_path: &root,
                    requested_name: "Other",
                    legacy_route: None,
                },
                &NoCancellation,
            )
            .unwrap();
        let Resolution::Found(unit) = outcome.result else {
            panic!("physical selected unit")
        };
        let names = callback
            .callback_unit_names
            .take()
            .expect("original selected name owners");
        let requested = names.requested.as_ptr();
        let declared = names.declared.as_ptr();
        let backing = ProviderSource::admit_cached_backing(&unit.source, &mut budget).unwrap();
        let provider = callback
            .build_owned_provider(unit, names, "Other", &mut budget, &NoCancellation)
            .unwrap();
        assert_eq!(
            provider.unit.requested_name.as_ptr(),
            requested,
            "provider construction consumes the original requested owner"
        );
        assert_eq!(
            provider.unit.declared_name.as_ptr(),
            declared,
            "provider construction consumes the original parsed owner"
        );
        let before = budget.snapshot();
        let promoted = ProviderSource::promote_owned(provider, backing, &mut budget).unwrap();
        assert_eq!(
            promoted.unit.requested_name.as_ptr(),
            requested,
            "selected requested name must move, not be cloned in provider promotion"
        );
        assert_eq!(
            promoted.unit.declared_name.as_ptr(),
            declared,
            "original parsed declaration must move with its minted credit"
        );
        assert_eq!(
            budget.snapshot().byte_work,
            before.byte_work,
            "provider promotion performs no second payload/name/context byte copy"
        );
    }

    #[test]
    fn unit_reason_owner_physical_missing_and_empty_outlive_resolver_and_reports() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        for requested in ["Missing", ""] {
            let (_dir, root, mut resolver) = physical_report_resolver();
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let observer = budget
                .admit_owned(0, 0, 0, || ())
                .unwrap()
                .into_shared(&mut budget)
                .unwrap();
            let baseline = budget.snapshot().retained_bytes;
            let out = resolver
                .unit_report_capability(
                    UnitResolveRequest {
                        importer_path: &root,
                        requested_name: requested,
                        legacy_route: None,
                    },
                    &mut budget,
                    &NoCancellation,
                )
                .unwrap();
            let (result, reports) = out.into_parts();
            assert!(
                result.owns_reason(),
                "actual production unit reason must retain its original construction owner"
            );
            let reason = match result.result() {
                Resolution::Unavailable { reason } | Resolution::Incomplete { reason, .. } => {
                    reason
                }
                _ => panic!("unit failure"),
            };
            let pointer = reason.as_ptr();
            let capacity = reason.capacity();
            assert!(!reason.is_empty());
            drop(reports);
            drop(resolver);
            drop(budget);
            assert_eq!(
                observer.budget_snapshot().retained_bytes,
                baseline
                    + capacity
                    + std::mem::size_of::<
                        AdmittedOwned<Resolution<crate::compile_time::AdmittedProviderSource>>,
                    >()
            );
            let reason = match result.result() {
                Resolution::Unavailable { reason } | Resolution::Incomplete { reason, .. } => {
                    reason
                }
                _ => unreachable!(),
            };
            assert_eq!(reason.as_ptr(), pointer);
            let spent = observer.budget_snapshot();
            drop(result);
            assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
            assert_eq!(observer.budget_snapshot().work, spent.work);
            assert_eq!(observer.budget_snapshot().byte_work, spent.byte_work);
        }
    }

    #[test]
    fn unit_candidate_owner_original_partial_fields_drop_independently_before_merge() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        let (dir, root, mut resolver) = physical_report_resolver();
        let a = dir.path().join("A.pas");
        let z = dir.path().join("Z.pas");
        fs::write(&a, "unit Wrong;").unwrap();
        fs::create_dir(&z).unwrap();
        let entries = [&a, &z]
            .into_iter()
            .map(|p| resolver.context.path_entry_for(p).unwrap())
            .collect();
        resolver
            .context
            .explicit_unit_entries
            .insert("broken".into(), entries);
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let observer = budget
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut budget)
            .unwrap();
        let baseline = budget.snapshot().retained_bytes;
        let mut call = resolver.callback_report_call();
        call.owning_unit_callback = true;
        call.owning_unit_reason = true;
        call.owning_unit_candidates = true;
        let out = CompileTimeResolver::unit_capability(
            &mut *call,
            UnitResolveRequest {
                importer_path: &root,
                requested_name: "Broken",
                legacy_route: None,
            },
            &mut budget,
            &NoCancellation,
        )
        .unwrap();
        assert!(
            matches!(out.result,Resolution::Incomplete {reason,candidates} if reason.is_empty() && candidates.is_empty())
        );
        let candidates = call.callback_unit_candidates.take().unwrap();
        let reason = call.callback_unit_reason.take().unwrap();
        let Some(CallbackReports::Built(reports)) = call.callback_reports.take() else {
            panic!("actual independently owned report construction")
        };
        let reason_pointer = reason.as_ptr();
        let payload = candidates.capacity() * std::mem::size_of::<ResolutionCandidate>()
            + candidates
                .iter()
                .map(|c| c.path.capacity() + c.declared_name.as_ref().map_or(0, String::capacity))
                .sum::<usize>();
        drop(call);
        drop(resolver);
        drop(budget);
        let before = observer.budget_snapshot();
        drop(candidates);
        assert_eq!(
            before.retained_bytes - observer.budget_snapshot().retained_bytes,
            payload + std::mem::size_of::<AdmittedOwned<Vec<ResolutionCandidate>>>()
        );
        assert_eq!(reason.as_ptr(), reason_pointer);
        assert_eq!(&*reason, "unit candidate is unauthorized");
        assert!(!reports.observations().is_empty());
        drop(reports);
        assert_eq!(
            observer.budget_snapshot().retained_bytes,
            baseline + reason.capacity() + std::mem::size_of::<AdmittedOwned<String>>()
        );
        drop(reason);
        assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
        assert_eq!(observer.budget_snapshot().work, before.work);
        assert_eq!(observer.budget_snapshot().byte_work, before.byte_work);
    }

    #[test]
    fn unit_candidate_owner_actual_constructor_refusal_peak_foreign_root_and_unwind() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        let (_dir, root, mut resolver) = physical_report_resolver();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let prior = resolver
            .unit_report_capability(
                UnitResolveRequest {
                    importer_path: &root,
                    requested_name: "Other",
                    legacy_route: None,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
        let Resolution::Found(provider) = prior.result() else {
            panic!("actual prior physical provider")
        };
        let path = &provider.unit.source.path;
        let mut vector = candidate_vector(1, &mut budget).unwrap();
        let name = owned_header_name(
            provider.unit.source.analysis_bytes().as_ref(),
            &mut budget,
            &NoCancellation,
        )
        .unwrap();
        let name_pointer = name.as_ref().unwrap().as_ptr();
        let candidate = unit_candidate_entry(path, name, &mut budget, &NoCancellation).unwrap();
        assert_eq!(
            candidate.declared_name.as_ref().unwrap().as_ptr(),
            name_pointer
        );
        let path_pointer = candidate.path.as_os_str().as_encoded_bytes().as_ptr();
        push_unit_candidate(&mut vector, candidate, &budget).unwrap();
        assert_eq!(
            vector[0].path.as_os_str().as_encoded_bytes().as_ptr(),
            path_pointer
        );
        let name = owned_header_name(
            provider.unit.source.analysis_bytes().as_ref(),
            &mut budget,
            &NoCancellation,
        )
        .unwrap();
        let before = budget.snapshot();
        budget.constrain_retained(before.retained_bytes).unwrap();
        struct Probe(std::cell::Cell<usize>);
        impl CancellationToken for Probe {
            fn is_cancelled(&self) -> bool {
                self.0.set(self.0.get() + 1);
                false
            }
        }
        let probe = Probe(0.into());
        assert!(matches!(
            unit_candidate_entry(path, name, &mut budget, &probe),
            Err(ResolverError::LimitExceeded { .. })
        ));
        assert_eq!(
            probe.0.get(),
            0,
            "path-copy body must not run after preconstruction refusal"
        );
        let before = budget.snapshot();
        assert!(matches!(
            candidate_vector(2, &mut budget),
            Err(ResolverError::LimitExceeded { .. })
        ));
        assert_eq!(
            budget.snapshot(),
            before,
            "receiving capacity refusal keeps old-plus-new peak unallocated"
        );
        assert_eq!(
            vector[0].path.as_os_str().as_encoded_bytes().as_ptr(),
            path_pointer
        );
        let mut other = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let before_other = other.snapshot();
        assert!(
            vector
                .map_value(&mut other, |candidates| {
                    Resolution::<LoadedSource>::Ambiguous { candidates }
                })
                .is_err()
        );
        assert_eq!(other.snapshot(), before_other);
        let mut fresh = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let observer = fresh
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut fresh)
            .unwrap();
        let baseline = fresh.snapshot().retained_bytes;
        struct PanicAfterCopy(std::cell::Cell<usize>);
        impl CancellationToken for PanicAfterCopy {
            fn is_cancelled(&self) -> bool {
                let n = self.0.get();
                self.0.set(n + 1);
                if n == 1 {
                    panic!("actual candidate path copied before callback unwind")
                }
                false
            }
        }
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = unit_candidate_entry(path, None, &mut fresh, &PanicAfterCopy(0.into()));
        }));
        assert!(unwind.is_err());
        assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
        assert!(fresh.snapshot().work > 0);
        assert!(fresh.snapshot().byte_work >= path.as_os_str().len());
        assert!(!provider.unit.source.bytes.is_empty());
        assert_eq!(provider.unit.declared_name, "Other");
        assert!(!prior.reports().observations().is_empty());
    }

    #[test]
    fn unit_candidate_owner_actual_partial_callback_cancel_every_poll_and_foreign_result() {
        use crate::compile_time::{AdmittedProviderSource, CompileTimeLimits, CompileTimeResolver};
        struct Poll {
            calls: std::sync::atomic::AtomicUsize,
            stop: usize,
        }
        impl CancellationToken for Poll {
            fn is_cancelled(&self) -> bool {
                self.calls.fetch_add(1, Ordering::Relaxed) >= self.stop
            }
        }
        let mut completed = false;
        let mut cancelled = 0;
        for stop in 0..700 {
            let (dir, root, mut resolver) = physical_report_resolver();
            let a = dir.path().join("A.pas");
            let z = dir.path().join("Z.pas");
            fs::write(&a, "unit Wrong;").unwrap();
            fs::create_dir(&z).unwrap();
            let entries = [&z, &a]
                .into_iter()
                .map(|p| resolver.context.path_entry_for(p).unwrap())
                .collect();
            resolver
                .context
                .explicit_unit_entries
                .insert("broken".into(), entries);
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let observer = budget
                .admit_owned(0, 0, 0, || ())
                .unwrap()
                .into_shared(&mut budget)
                .unwrap();
            let baseline = budget.snapshot().retained_bytes;
            let prior = resolver
                .unit_report_capability(
                    UnitResolveRequest {
                        importer_path: &root,
                        requested_name: "Other",
                        legacy_route: None,
                    },
                    &mut budget,
                    &NoCancellation,
                )
                .unwrap();
            match resolver.unit_report_capability(
                UnitResolveRequest {
                    importer_path: &root,
                    requested_name: "Broken",
                    legacy_route: None,
                },
                &mut budget,
                &Poll {
                    calls: 0.into(),
                    stop,
                },
            ) {
                Err(ResolverError::Cancelled) => cancelled += 1,
                Ok(out) => {
                    assert!(
                        matches!(out.result(),Resolution::Incomplete {reason,candidates} if reason=="unit candidate is unauthorized" && candidates.len()==1 && candidates[0].declared_name.as_deref()==Some("Wrong"))
                    );
                    let (result, reports) = out.into_parts();
                    assert!(result.owns_candidates());
                    let mut other =
                        CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
                    let mut receiver = other
                        .admit_owned(0, 0, 0, || None::<Resolution<AdmittedProviderSource>>)
                        .unwrap();
                    let before = other.snapshot();
                    assert!(
                        result
                            .move_fields_into(&mut receiver, &other, |to, value| *to = Some(value))
                            .is_err()
                    );
                    assert_eq!(other.snapshot(), before);
                    assert!(receiver.is_none());
                    assert!(!reports.observations().is_empty());
                    drop(reports);
                    completed = true;
                }
                Err(error) => panic!("unexpected callback result at {stop}: {error:?}"),
            }
            assert!(resolver.callback_unit_candidates.is_none());
            assert!(resolver.callback_unit_reason.is_none());
            let spent = budget.snapshot();
            drop(resolver);
            drop(budget);
            assert!(matches!(prior.result(),Resolution::Found(p) if p.unit.declared_name=="Other"));
            assert!(!prior.reports().observations().is_empty());
            drop(prior);
            assert_eq!(
                observer.budget_snapshot().retained_bytes,
                baseline,
                "no candidate/name/reason/control leak at poll {stop}"
            );
            assert_eq!(observer.budget_snapshot().work, spent.work);
            assert_eq!(observer.budget_snapshot().byte_work, spent.byte_work);
            if completed {
                break;
            }
        }
        assert!(completed);
        assert!(cancelled > 20);
    }

    #[test]
    fn unit_candidate_owner_partial_original_pointers_capacity_and_reason_move_without_copy() {
        use crate::compile_time::{AdmittedProviderSource, CompileTimeLimits, CompileTimeResolver};
        let (dir, root, mut resolver) = physical_report_resolver();
        let a = dir.path().join("A.pas");
        let b = dir.path().join("B.pas");
        let z = dir.path().join("Z.pas");
        fs::write(&a, "unit Wrong;").unwrap();
        fs::write(&b, "not a unit header").unwrap();
        fs::create_dir(&z).unwrap();
        let entries = [&z, &b, &a]
            .into_iter()
            .map(|p| resolver.context.path_entry_for(p).unwrap())
            .collect();
        resolver
            .context
            .explicit_unit_entries
            .insert("broken".into(), entries);
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let observer = budget
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut budget)
            .unwrap();
        let baseline = budget.snapshot().retained_bytes;
        let mut call = resolver.callback_report_call();
        call.owning_unit_callback = true;
        call.owning_unit_reason = true;
        call.owning_unit_candidates = true;
        let out = CompileTimeResolver::unit_capability(
            &mut *call,
            UnitResolveRequest {
                importer_path: &root,
                requested_name: "Broken",
                legacy_route: None,
            },
            &mut budget,
            &NoCancellation,
        )
        .unwrap();
        let vector = call.callback_unit_candidates.as_ref().unwrap();
        assert_eq!(vector.len(), 2);
        assert_eq!(vector.capacity(), 3);
        assert_eq!(vector[0].declared_name.as_deref(), Some("Wrong"));
        assert!(vector[1].declared_name.is_none());
        let vec_pointer = vector.as_ptr();
        let path_pointer = vector[0].path.as_os_str().as_encoded_bytes().as_ptr();
        let name_pointer = vector[0].declared_name.as_ref().unwrap().as_ptr();
        let payload = vector.capacity() * std::mem::size_of::<ResolutionCandidate>()
            + vector
                .iter()
                .map(|c| c.path.capacity() + c.declared_name.as_ref().map_or(0, String::capacity))
                .sum::<usize>();
        let reason = call.callback_unit_reason.as_ref().unwrap();
        let reason_pointer = reason.as_ptr();
        let reason_capacity = reason.capacity();
        let before = budget.snapshot();
        let out = call.finish(out.result, &mut budget).unwrap();
        assert_eq!(
            budget.snapshot().byte_work,
            before.byte_work,
            "callback candidate and original reason transfer cannot clone payloads"
        );
        let (result, reports) = out.into_parts();
        assert!(result.owns_candidates());
        assert!(result.owns_reason());
        let mut receiver = budget
            .admit_owned(0, 0, 0, || None::<Resolution<AdmittedProviderSource>>)
            .unwrap();
        let before = budget.snapshot();
        result
            .move_fields_into(&mut receiver, &budget, |to, value| *to = Some(value))
            .unwrap();
        assert_eq!(budget.snapshot().byte_work, before.byte_work);
        drop(reports);
        drop(resolver);
        drop(budget);
        assert_eq!(
            observer.budget_snapshot().retained_bytes,
            baseline
                + payload
                + reason_capacity
                + std::mem::size_of::<AdmittedOwned<Option<Resolution<AdmittedProviderSource>>>>()
        );
        let Some(Resolution::Incomplete { reason, candidates }) = &*receiver else {
            panic!("real incomplete receiving fields")
        };
        assert_eq!(reason.as_ptr(), reason_pointer);
        assert_eq!(candidates.as_ptr(), vec_pointer);
        assert_eq!(
            candidates[0].path.as_os_str().as_encoded_bytes().as_ptr(),
            path_pointer
        );
        assert_eq!(
            candidates[0].declared_name.as_ref().unwrap().as_ptr(),
            name_pointer
        );
        let spent = observer.budget_snapshot();
        drop(receiver);
        assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
        assert_eq!(observer.budget_snapshot().work, spent.work);
        assert_eq!(observer.budget_snapshot().byte_work, spent.byte_work);
    }

    #[test]
    fn unit_candidate_owner_case_ambiguity_and_empty_vectors_are_original_owned_storage() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        for case in [true, false] {
            let (dir, root, mut resolver) = physical_report_resolver();
            fs::create_dir(dir.path().join("SPACE")).unwrap();
            fs::create_dir(dir.path().join("Space")).unwrap();
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let observer = budget
                .admit_owned(0, 0, 0, || ())
                .unwrap()
                .into_shared(&mut budget)
                .unwrap();
            let baseline = budget.snapshot().retained_bytes;
            let out = resolver
                .unit_report_capability(
                    UnitResolveRequest {
                        importer_path: &root,
                        requested_name: if case { "space.Other" } else { "" },
                        legacy_route: None,
                    },
                    &mut budget,
                    &NoCancellation,
                )
                .unwrap();
            let (result, reports) = out.into_parts();
            assert!(result.owns_candidates());
            let candidates = match result.result() {
                Resolution::Ambiguous { candidates } if case => candidates,
                Resolution::Incomplete { candidates, .. } if !case => candidates,
                _ => panic!("actual nested case/empty result"),
            };
            assert_eq!(candidates.len(), if case { 2 } else { 0 });
            assert!(candidates.iter().all(|c| c.declared_name.is_none()));
            let vector_pointer = candidates.as_ptr();
            let payload = candidates.capacity() * std::mem::size_of::<ResolutionCandidate>()
                + candidates.iter().map(|c| c.path.capacity()).sum::<usize>()
                + match result.result() {
                    Resolution::Incomplete { reason, .. } => reason.capacity(),
                    _ => 0,
                };
            drop(reports);
            drop(resolver);
            drop(budget);
            assert_eq!(
                observer.budget_snapshot().retained_bytes,
                baseline
                    + payload
                    + std::mem::size_of::<
                        AdmittedOwned<Resolution<crate::compile_time::AdmittedProviderSource>>,
                    >()
            );
            assert!(
                matches!(result.result(),Resolution::Ambiguous {candidates}|Resolution::Incomplete {candidates,..} if candidates.as_ptr()==vector_pointer)
            );
            assert!(result.into_unmigrated().is_err());
            assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
        }
    }

    #[test]
    fn unit_candidate_owner_physical_ambiguity_survives_cache_report_and_budget_exit() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        let (dir, root, mut resolver) = physical_report_resolver();
        let a = dir.path().join("A.pas");
        let z = dir.path().join("Z.pas");
        fs::write(&a, "unit Other; interface end.").unwrap();
        fs::write(&z, "unit Other; interface end.").unwrap();
        let entries = [&z, &a]
            .into_iter()
            .map(|p| resolver.context.path_entry_for(p).unwrap())
            .collect();
        resolver
            .context
            .explicit_unit_entries
            .insert("other".into(), entries);
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let observer = budget
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut budget)
            .unwrap();
        let baseline = budget.snapshot().retained_bytes;
        let out = resolver
            .unit_report_capability(
                UnitResolveRequest {
                    importer_path: &root,
                    requested_name: "Other",
                    legacy_route: None,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
        let (result, reports) = out.into_parts();
        let Resolution::Ambiguous { candidates } = result.result() else {
            panic!("real multiple valid physical candidates")
        };
        assert_eq!(candidates.len(), 2);
        assert!(candidates[0].path.ends_with("A.pas"));
        assert!(candidates[1].path.ends_with("Z.pas"));
        let backing = candidates.as_ptr();
        let path = candidates[0].path.as_os_str().as_encoded_bytes().as_ptr();
        let name = candidates[0].declared_name.as_ref().unwrap().as_ptr();
        let payload = candidates.capacity() * std::mem::size_of::<ResolutionCandidate>()
            + candidates
                .iter()
                .map(|c| c.path.capacity() + c.declared_name.as_ref().map_or(0, String::capacity))
                .sum::<usize>();
        drop(reports);
        drop(resolver);
        drop(budget);
        assert_eq!(
            observer.budget_snapshot().retained_bytes,
            baseline
                + payload
                + std::mem::size_of::<
                    AdmittedOwned<Resolution<crate::compile_time::AdmittedProviderSource>>,
                >(),
            "escaped candidates must retain exact original vector/path/name credits"
        );
        let Resolution::Ambiguous { candidates } = result.result() else {
            unreachable!()
        };
        assert_eq!(candidates.as_ptr(), backing);
        assert_eq!(
            candidates[0].path.as_os_str().as_encoded_bytes().as_ptr(),
            path
        );
        assert_eq!(candidates[0].declared_name.as_ref().unwrap().as_ptr(), name);
        let spent = observer.budget_snapshot();
        drop(result);
        assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
        assert_eq!(observer.budget_snapshot().work, spent.work);
        assert_eq!(observer.budget_snapshot().byte_work, spent.byte_work);
    }

    #[test]
    fn include_r18_first_hit_unwind_and_foreign_empty_failure_keep_selected_source() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        struct PanicAt {
            calls: std::sync::atomic::AtomicUsize,
            stop: usize,
        }
        impl CancellationToken for PanicAt {
            fn is_cancelled(&self) -> bool {
                if self.calls.fetch_add(1, Ordering::Relaxed) >= self.stop {
                    panic!("actual first-hit include callback unwind")
                }
                false
            }
        }
        let mut done = false;
        let mut panics = 0;
        for stop in 0..400 {
            let (_dir, root, mut resolver) = physical_report_resolver();
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let observer = budget
                .admit_owned(0, 0, 0, || ())
                .unwrap()
                .into_shared(&mut budget)
                .unwrap();
            let baseline = budget.snapshot().retained_bytes;
            let prior = resolver
                .include_capability(
                    IncludeResolveRequest {
                        including_path: &root,
                        byte_range: 0..0,
                        requested_name: "absent.inc",
                        legacy_route: None,
                    },
                    &mut budget,
                    &NoCancellation,
                )
                .unwrap();
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                resolver.include_capability(
                    IncludeResolveRequest {
                        including_path: &root,
                        byte_range: 0..0,
                        requested_name: "value.inc",
                        legacy_route: None,
                    },
                    &mut budget,
                    &PanicAt {
                        calls: 0.into(),
                        stop,
                    },
                )
            })) {
                Err(_) => panics += 1,
                Ok(Ok(out)) => {
                    assert!(matches!(out.result(), Resolution::Found(_)));
                    drop(out);
                    done = true;
                }
                Ok(Err(other)) => panic!("unexpected error: {other:?}"),
            }
            assert!(!resolver.owning_include_callback);
            assert!(resolver.callback_include_reason.is_none());
            let spent = budget.snapshot();
            drop(resolver);
            drop(budget);
            assert!(
                matches!(prior.result(),Resolution::Unavailable {reason} if reason.contains("absent.inc"))
            );
            drop(prior);
            assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
            assert_eq!(observer.budget_snapshot().work, spent.work);
            assert_eq!(observer.budget_snapshot().byte_work, spent.byte_work);
            if done {
                break;
            }
        }
        assert!(done);
        assert!(panics > 10);
        let (dir, root, mut resolver) = physical_report_resolver();
        fs::create_dir(dir.path().join("bad.inc")).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let prior = resolver
            .include_capability(
                IncludeResolveRequest {
                    including_path: &root,
                    byte_range: 0..0,
                    requested_name: "value.inc",
                    legacy_route: None,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
        let out = resolver
            .include_capability(
                IncludeResolveRequest {
                    including_path: &root,
                    byte_range: 0..0,
                    requested_name: "bad.inc",
                    legacy_route: None,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
        assert!(
            matches!(out.result(),Resolution::Incomplete {reason,candidates} if !reason.is_empty() && candidates.capacity()==0)
        );
        let (result, reports) = out.into_parts();
        let mut other = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut receiver = other
            .admit_owned(0, 0, 0, || None::<Resolution<LoadedSource>>)
            .unwrap();
        let before = other.snapshot();
        assert!(
            result
                .move_reason_into(&mut receiver, &other, |to, value| *to = Some(value))
                .is_err()
        );
        assert_eq!(other.snapshot(), before);
        assert!(receiver.is_none());
        assert!(!reports.observations().is_empty());
        assert!(
            matches!(prior.result(),Resolution::Found(source) if source.bytes.as_ref()==b"{physical include}")
        );
    }

    #[test]
    fn include_r18_first_hit_overlay_dedup_and_exact_empty_case_warning_preserve_source_owners() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        for overlay in [false, true] {
            let (dir, root, mut resolver) = physical_report_resolver();
            let later = dir.path().join("later");
            fs::create_dir(&later).unwrap();
            fs::write(later.join("value.inc"), "{later physical include}").unwrap();
            resolver.context.include_paths = vec![later.clone(), later.clone()];
            resolver.context.include_path_entries =
                vec![resolver.context.path_entry_for(&later).unwrap(); 2];
            if overlay {
                resolver.store.insert_overlay(
                    dir.path().join("value.inc"),
                    7,
                    Arc::<[u8]>::from(&b"{overlay first}"[..]),
                );
            }
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let observer = budget
                .admit_owned(0, 0, 0, || ())
                .unwrap()
                .into_shared(&mut budget)
                .unwrap();
            let baseline = budget.snapshot().retained_bytes;
            let out = resolver
                .include_capability(
                    IncludeResolveRequest {
                        including_path: &root,
                        byte_range: 0..0,
                        requested_name: "value.inc",
                        legacy_route: None,
                    },
                    &mut budget,
                    &NoCancellation,
                )
                .unwrap();
            let Resolution::Found(source) = out.result() else {
                panic!("first source wins")
            };
            assert_eq!(source.path, dir.path().join("value.inc"));
            if overlay {
                assert_eq!(source.bytes.as_ref(), b"{overlay first}");
            }
            assert!(!out.reports().observations().iter().any(|o|matches!(o,ResolutionObservation::Payload {path,..} if path==&later.join("value.inc"))));
            let backing = source.compile_time_backing.as_ref().unwrap().clone();
            let source_pointer = source.bytes.as_ptr();
            fs::create_dir(dir.path().join("Mixed")).unwrap();
            fs::create_dir(dir.path().join("MIXED")).unwrap();
            fs::write(dir.path().join("Mixed/body.inc"), "{one}").unwrap();
            fs::write(dir.path().join("MIXED/body.inc"), "{two}").unwrap();
            let failure = resolver
                .include_capability(
                    IncludeResolveRequest {
                        including_path: &root,
                        byte_range: 0..0,
                        requested_name: "mixed/body.inc",
                        legacy_route: None,
                    },
                    &mut budget,
                    &NoCancellation,
                )
                .unwrap();
            let Resolution::Incomplete { reason, candidates } = failure.result() else {
                panic!("nested physical case ambiguity stays incomplete")
            };
            assert_eq!(reason, "include mixed/body.inc lookup was incomplete");
            assert!(candidates.is_empty());
            assert_eq!(candidates.capacity(), 0);
            let mut paths = vec![dir.path().join("Mixed"), dir.path().join("MIXED")];
            paths.sort_by_key(|p| path_key(p));
            let warning = format!(
                "include mixed/body.inc has ambiguous case-insensitive path components: {}",
                ReportPaths(&paths)
            );
            assert_eq!(failure.reports().warnings(), &[warning]);
            let reason_pointer = reason.as_ptr();
            drop(resolver);
            drop(budget);
            assert!(
                matches!(out.result(),Resolution::Found(source) if source.bytes.as_ptr()==source_pointer)
            );
            assert!(
                matches!(failure.result(),Resolution::Incomplete {reason,candidates} if reason.as_ptr()==reason_pointer && candidates.is_empty())
            );
            drop(out);
            drop(failure);
            assert_eq!(backing.bytes.as_ptr(), source_pointer);
            assert!(
                observer.budget_snapshot().retained_bytes > baseline,
                "selected shared source backing survives discarded lookup/output controls"
            );
            let spent = observer.budget_snapshot();
            drop(backing);
            assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
            assert_eq!(observer.budget_snapshot().work, spent.work);
            assert_eq!(observer.budget_snapshot().byte_work, spent.byte_work);
        }
    }

    #[test]
    fn source_load_scratch_missing_request_refunds_before_live_cache_and_reports_drop() {
        use crate::compile_time::CompileTimeLimits;
        let (dir, _, mut resolver) = physical_report_resolver();
        let path = dir.path().join("missing.pas");
        let entry = resolver.context.path_entry_for(&path).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut call = resolver.store_call(&mut budget).unwrap();
        let before = budget.snapshot();
        let error = call
            .load_candidate_with_limit(&path, &entry, None, SourceKind::Unit, 4096, &NoCancellation)
            .unwrap_err();
        assert!(matches!(error, SourceStoreError::NotFound { .. }));
        drop(error);
        assert!(!call.report.observations.is_empty());
        // Actual report buffers and original receipts die together. Keep the
        // actual cache root alive to detect request scratch misbilled to it.
        call.report = ResolutionReport {
            observations: Vec::new(),
            warnings: Vec::new(),
            complete: true,
            incomplete_reasons: Vec::new(),
        };
        call.report_owners = ReportOwners::default();
        assert_eq!(
            budget.snapshot().retained_bytes,
            before.retained_bytes,
            "dead canonical/local-entry request scratch must not remain on live source cache"
        );
        assert!(budget.snapshot().work > before.work);
        assert!(budget.snapshot().byte_work > before.byte_work);
    }

    #[test]
    fn loaded_cache_owner_actual_replacement_does_not_accumulate_dead_key_invoices() {
        use crate::compile_time::CompileTimeLimits;
        let (dir, _, mut resolver) = physical_report_resolver();
        let path = dir.path().join("Other.pas");
        let entry = resolver.context.path_entry_for(&path).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut call = resolver.store_call(&mut budget).unwrap();
        let first = call
            .load_candidate_with_limit(&path, &entry, None, SourceKind::Unit, 4096, &NoCancellation)
            .unwrap();
        drop(first);
        call.report = ResolutionReport {
            observations: Vec::new(),
            warnings: Vec::new(),
            complete: true,
            incomplete_reasons: Vec::new(),
        };
        call.report_owners = ReportOwners::default();
        let before = budget.snapshot();
        let second = call
            .load_candidate_with_limit(&path, &entry, None, SourceKind::Unit, 4096, &NoCancellation)
            .unwrap();
        drop(second);
        call.report = ResolutionReport {
            observations: Vec::new(),
            warnings: Vec::new(),
            complete: true,
            incomplete_reasons: Vec::new(),
        };
        call.report_owners = ReportOwners::default();
        assert_eq!(
            budget.snapshot().retained_bytes,
            before.retained_bytes,
            "actual replaced key/value die; receiving capacity is unchanged, no cumulative cache key invoice"
        );
        assert!(budget.snapshot().work > before.work);
        assert!(budget.snapshot().byte_work > before.byte_work);
    }

    #[test]
    fn include_reuse_owner_actual_seen_storage_is_retained_and_duplicates_do_not_accumulate() {
        use crate::compile_time::CompileTimeLimits;
        let (dir, _, mut resolver) = physical_report_resolver();
        let path = dir.path().join("value.inc");
        let entry = resolver.context.path_entry_for(&path).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut call = resolver.store_call(&mut budget).unwrap();
        let original = call
            .load_candidate_with_limit(
                &path,
                &entry,
                None,
                SourceKind::Include,
                4096,
                &NoCancellation,
            )
            .unwrap();
        let before = budget.snapshot();
        let first = call
            .load_include_candidate(&path, &entry, None, &NoCancellation)
            .unwrap();
        assert_eq!(first.bytes.as_ptr(), original.bytes.as_ptr());
        drop(first);
        let seen = budget.snapshot();
        assert!(
            seen.retained_bytes > before.retained_bytes,
            "actual persistent seen key/set must retain independent original owners after query/source scratch drops"
        );
        let repeated = call
            .load_include_candidate(&path, &entry, None, &NoCancellation)
            .unwrap();
        assert_eq!(repeated.bytes.as_ptr(), original.bytes.as_ptr());
        drop(repeated);
        assert_eq!(
            budget.snapshot().retained_bytes,
            seen.retained_bytes,
            "duplicate has no dead key invoice"
        );
        assert!(budget.snapshot().work > seen.work);
        assert!(budget.snapshot().byte_work > seen.byte_work);
    }

    #[test]
    fn include_limit_error_owner_actual_file_limit_survives_resolver_cache_and_budget_exit() {
        use crate::compile_time::CompileTimeLimits;
        let (dir, _, mut resolver) = physical_report_resolver();
        let path = dir.path().join("value.inc");
        let entry = resolver.context.path_entry_for(&path).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let observer = budget
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut budget)
            .unwrap();
        let baseline = budget.snapshot().retained_bytes;
        let mut call = resolver.store_call(&mut budget).unwrap();
        call.limits.max_include_files = 0;
        let error = call
            .load_include_candidate(&path, &entry, None, &NoCancellation)
            .unwrap_err();
        let expected = format!(
            "{:?}",
            SourceStoreError::Incomplete {
                path: path.clone(),
                reason: "include file limit (0) reached".into()
            }
        );
        assert_eq!(format!("{error:?}"), expected);
        let spent = budget.snapshot();
        drop(call);
        drop(resolver);
        drop(budget);
        assert_eq!(format!("{error:?}"), expected);
        assert!(
            observer.budget_snapshot().retained_bytes > baseline,
            "actual escaped path/reason remain live and must retain their original receipts"
        );
        drop(error);
        assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
        assert_eq!(observer.budget_snapshot().work, spent.work);
        assert_eq!(observer.budget_snapshot().byte_work, spent.byte_work);
    }

    #[test]
    fn include_limit_error_owner_actual_byte_variants_transfer_and_warning_lifetimes() {
        use crate::compile_time::CompileTimeLimits;
        for cached in [false, true] {
            for exceeded in [false, true] {
                if exceeded && !cached {
                    continue;
                }
                let (dir, _, mut resolver) = physical_report_resolver();
                let path = dir.path().join("value.inc");
                let entry = resolver.context.path_entry_for(&path).unwrap();
                let mut budget =
                    CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
                let observer = budget
                    .admit_owned(0, 0, 0, || ())
                    .unwrap()
                    .into_shared(&mut budget)
                    .unwrap();
                let baseline = budget.snapshot().retained_bytes;
                let mut call = resolver.store_call(&mut budget).unwrap();
                let source = if cached {
                    Some(
                        call.load_candidate_with_limit(
                            &path,
                            &entry,
                            None,
                            SourceKind::Include,
                            4096,
                            &NoCancellation,
                        )
                        .unwrap(),
                    )
                } else {
                    None
                };
                call.limits.max_include_bytes = if exceeded { 1 } else { 0 };
                let error = call
                    .load_include_candidate(&path, &entry, None, &NoCancellation)
                    .unwrap_err();
                let SourceStoreError::Incomplete {
                    path: original_path,
                    reason,
                } = error.error_ref()
                else {
                    panic!("owned byte error expected")
                };
                assert_eq!(original_path, &path);
                let pp = original_path.as_os_str().as_encoded_bytes().as_ptr();
                let rp = reason.as_ptr();
                assert_eq!(
                    reason,
                    &format!(
                        "include byte limit ({}) {}",
                        call.limits.max_include_bytes,
                        if exceeded { "exceeded" } else { "reached" }
                    )
                );
                let expected = format!("include candidate could not be loaded: {error:?}");
                let mut receiver = budget.admit_owned(0, 0, 0, || None).unwrap();
                error
                    .move_into(&mut receiver, &budget, |to, error| *to = Some(error))
                    .unwrap();
                let SourceStoreError::Incomplete { path, reason } = receiver.as_ref().unwrap()
                else {
                    unreachable!()
                };
                assert_eq!(path.as_os_str().as_encoded_bytes().as_ptr(), pp);
                assert_eq!(reason.as_ptr(), rp);
                // Another actual helper error is independently converted through the
                // production warning consumer; original receiver remains credited.
                let error = call
                    .load_include_candidate(path, &entry, None, &NoCancellation)
                    .unwrap_err();
                call.include_error_warning(error, &NoCancellation).unwrap();
                assert_eq!(call.report.warnings.last().unwrap(), &expected);
                assert_eq!((call.include_files, call.include_bytes), (0, 0));
                let cache = call
                    .compile_time_store
                    .as_ref()
                    .unwrap()
                    .view
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                assert_eq!(cache.loaded.len(), usize::from(cached));
                assert_eq!(cache.include_seen.len(), 0);
                drop(cache);
                drop(call);
                let report = resolver.finish_admitted(&mut budget).unwrap();
                drop(source);
                drop(budget);
                assert_eq!(report.report().warnings.last().unwrap(), &expected);
                assert!(observer.budget_snapshot().retained_bytes > baseline);
                drop(receiver);
                assert!(
                    observer.budget_snapshot().retained_bytes > baseline,
                    "actual warning remains independently owned after original error dies"
                );
                drop(report);
                assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
            }
        }
    }

    #[test]
    fn include_limit_error_owner_actual_constructor_and_warning_cancel_unwind_sweeps() {
        use crate::compile_time::CompileTimeLimits;
        struct Poll {
            calls: std::sync::atomic::AtomicUsize,
            stop: usize,
            panic: bool,
        }
        impl CancellationToken for Poll {
            fn is_cancelled(&self) -> bool {
                let stop = self.calls.fetch_add(1, Ordering::Relaxed) >= self.stop;
                if stop && self.panic {
                    panic!("include limit constructor/consumer injection")
                }
                stop
            }
        }
        for byte_exceeded in [false, true] {
            for conversion in [false, true] {
                for panic in [false, true] {
                    let mut finished = false;
                    let mut failures = 0;
                    for stop in 0..300 {
                        let (dir, _, mut resolver) = physical_report_resolver();
                        let path = dir.path().join("value.inc");
                        let entry = resolver.context.path_entry_for(&path).unwrap();
                        let mut budget =
                            CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
                        let mut call = resolver.store_call(&mut budget).unwrap();
                        let source = call
                            .load_candidate_with_limit(
                                &path,
                                &entry,
                                None,
                                SourceKind::Include,
                                4096,
                                &NoCancellation,
                            )
                            .unwrap();
                        if byte_exceeded {
                            call.limits.max_include_bytes = 1;
                        } else {
                            call.limits.max_include_files = 0;
                        }
                        let error = if conversion {
                            Some(
                                call.load_include_candidate(&path, &entry, None, &NoCancellation)
                                    .unwrap_err(),
                            )
                        } else {
                            None
                        };
                        let before = budget.snapshot();
                        let token = Poll {
                            calls: 0.into(),
                            stop,
                            panic,
                        };
                        let outcome =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                if let Some(error) = error {
                                    call.include_error_warning(error, &token)
                                } else {
                                    match call.load_include_candidate(&path, &entry, None, &token) {
                                        Err(error)
                                            if matches!(
                                                error.error_ref(),
                                                SourceStoreError::Incomplete { .. }
                                            ) =>
                                        {
                                            Ok(())
                                        }
                                        Err(error) => Err(error
                                            .refusal()
                                            .expect("only cancellation expected")),
                                        Ok(_) => panic!("file limit must refuse"),
                                    }
                                }
                            }));
                        match outcome {
                            Err(_) => {
                                assert!(panic);
                                failures += 1;
                            }
                            Ok(Err(ResolverError::Cancelled)) => {
                                assert!(!panic);
                                failures += 1;
                            }
                            Ok(Ok(())) => {
                                finished = true;
                            }
                            Ok(Err(error)) => panic!("unexpected {error:?}"),
                        }
                        let cache = call
                            .compile_time_store
                            .as_ref()
                            .unwrap()
                            .view
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        assert_eq!(cache.loaded.len(), 1);
                        assert_eq!(cache.loaded.capacity(), 3);
                        assert_eq!(cache.include_seen.len(), 0);
                        assert_eq!(cache.include_seen.capacity(), 0);
                        assert_eq!(
                            cache.loaded.get(&source.id).unwrap().bytes.as_ptr(),
                            source.bytes.as_ptr()
                        );
                        drop(cache);
                        assert_eq!((call.include_files, call.include_bytes), (0, 0));
                        assert!(budget.snapshot().work >= before.work);
                        assert!(budget.snapshot().byte_work >= before.byte_work);
                        if !conversion {
                            assert_eq!(
                                budget.snapshot().retained_bytes,
                                before.retained_bytes,
                                "no dead query/error receipts after poll {stop}"
                            );
                        }
                        let retry = call
                            .load_include_candidate(&path, &entry, None, &NoCancellation)
                            .unwrap_err();
                        assert!(matches!(
                            retry.error_ref(),
                            SourceStoreError::Incomplete { .. }
                        ));
                        drop(retry);
                        if finished {
                            break;
                        }
                    }
                    assert!(finished);
                    assert!(failures > 2);
                }
            }
        }
    }

    #[test]
    fn include_limit_error_owner_actual_refusal_foreign_transfer_and_owning_outcome() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        let (dir, root, mut resolver) = physical_report_resolver();
        let path = dir.path().join("value.inc");
        let entry = resolver.context.path_entry_for(&path).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let observer = budget
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut budget)
            .unwrap();
        let baseline = budget.snapshot().retained_bytes;
        resolver.limits.max_include_files = 0;
        let mut call = resolver.store_call(&mut budget).unwrap();
        let before = budget.snapshot();
        let error = call
            .load_include_candidate(&path, &entry, None, &NoCancellation)
            .unwrap_err();
        let expected = format!("include candidate could not be loaded: {error:?}");
        let after = budget.snapshot();
        let mut foreign = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut receiver = foreign.admit_owned(0, 0, 0, || None).unwrap();
        let foreign_before = foreign.snapshot();
        assert!(
            error
                .move_into(&mut receiver, &foreign, |to, error| *to = Some(error))
                .is_err()
        );
        assert!(receiver.is_none());
        assert_eq!(foreign.snapshot(), foreign_before);
        drop(call);
        let outcome = resolver
            .include_capability(
                IncludeResolveRequest {
                    including_path: &root,
                    byte_range: 0..0,
                    requested_name: "value.inc",
                    legacy_route: None,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
        assert!(
            matches!(outcome.result(),Resolution::Incomplete {reason,candidates} if reason=="include value.inc lookup was incomplete" && candidates.is_empty())
        );
        assert!(
            outcome
                .reports()
                .warnings()
                .iter()
                .any(|warning| warning == &expected)
        );
        let report = resolver.finish_admitted(&mut budget).unwrap();
        drop(budget);
        assert!(
            outcome
                .reports()
                .warnings()
                .iter()
                .any(|warning| warning == &expected)
        );
        drop(report);
        assert!(observer.budget_snapshot().retained_bytes > baseline);
        drop(outcome);
        assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
        // Real helper construction work refusal occurs at its last work admission
        // (the original reason formatter), not after formatting a plain error.
        let cost = after.work - before.work;
        let (_dir, _, mut resolver) = physical_report_resolver();
        let mut probe = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        drop(resolver.store_call(&mut probe).unwrap());
        let setup = probe.snapshot().work;
        let (dir, _, mut resolver) = physical_report_resolver();
        let path = dir.path().join("value.inc");
        let entry = resolver.context.path_entry_for(&path).unwrap();
        let mut tight = CompileTimeBudget::new(
            CompileTimeLimits {
                max_work: setup + cost - 1,
                ..Default::default()
            },
            &NoCancellation,
        );
        let mut call = resolver.store_call(&mut tight).unwrap();
        call.limits.max_include_files = 0;
        assert!(
            matches!(call.load_include_candidate(&path,&entry,None,&NoCancellation),Err(error) if matches!(error.error_ref(),SourceStoreError::RootLimit {..}))
        );
        assert!(call.report.warnings.is_empty());
        assert_eq!((call.include_files, call.include_bytes), (0, 0));
        drop(call);
        let (dir, _, mut resolver) = physical_report_resolver();
        let path = dir.path().join("value.inc");
        let entry = resolver.context.path_entry_for(&path).unwrap();
        let mut budget = CompileTimeBudget::new(
            CompileTimeLimits {
                max_work: setup + cost + 1,
                ..Default::default()
            },
            &NoCancellation,
        );
        let mut call = resolver.store_call(&mut budget).unwrap();
        call.limits.max_include_files = 0;
        let error = call
            .load_include_candidate(&path, &entry, None, &NoCancellation)
            .unwrap_err();
        assert!(matches!(
            error.error_ref(),
            SourceStoreError::Incomplete { .. }
        ));
        let live = budget.snapshot();
        assert!(matches!(
            call.include_error_warning(error, &NoCancellation),
            Err(ResolverError::LimitExceeded { .. })
        ));
        assert!(call.report.warnings.is_empty());
        assert_eq!((call.include_files, call.include_bytes), (0, 0));
        assert_eq!(
            budget.snapshot().work,
            live.work,
            "refused sizing/formatting does not refund prior construction work"
        );
    }

    #[test]
    fn include_publication_actual_both_receiving_refusals_after_read_report_preserve_caches_and_counters()
     {
        use crate::compile_time::{BudgetSnapshot, CompileTimeLimits};
        struct Trace<'a> {
            owner: &'a AdmittedShared<()>,
            samples: std::cell::RefCell<Vec<BudgetSnapshot>>,
        }
        impl CancellationToken for Trace<'_> {
            fn is_cancelled(&self) -> bool {
                self.samples.borrow_mut().push(self.owner.budget_snapshot());
                false
            }
        }
        fn run(limits: CompileTimeLimits, refusal: bool) -> (usize, usize) {
            let (dir, _, mut resolver) = physical_report_resolver();
            let mut budget = CompileTimeBudget::new(limits, &NoCancellation);
            let observer = budget
                .admit_owned(0, 0, 0, || ())
                .unwrap()
                .into_shared(&mut budget)
                .unwrap();
            let baseline = budget.snapshot().retained_bytes;
            let mut call = resolver.store_call(&mut budget).unwrap();
            let mut held = Vec::new();
            for i in 0..3 {
                let p = dir.path().join(format!("Prior{i}.inc"));
                fs::write(&p, format!("{{prior {i}}}")).unwrap();
                let e = call.context.path_entry_for(&p).unwrap();
                held.push(
                    call.load_include_candidate(&p, &e, None, &NoCancellation)
                        .unwrap(),
                );
            }
            let root = call.compile_time_store.as_ref().unwrap().clone();
            let keys = {
                let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
                assert_eq!(cache.loaded.len(), cache.loaded.capacity());
                assert_eq!(cache.include_seen.len(), cache.include_seen.capacity());
                held.iter()
                    .map(|s| {
                        (
                            s.id.clone(),
                            cache
                                .loaded
                                .get_key_value(&s.id)
                                .unwrap()
                                .0
                                .0
                                .as_str()
                                .as_ptr(),
                            cache.include_seen.get(&s.id).unwrap().0.as_str().as_ptr(),
                        )
                    })
                    .collect::<Vec<_>>()
            };
            let counters = (call.include_files, call.include_bytes);
            let before = budget.snapshot();
            let p = dir.path().join("Next.inc");
            fs::write(&p, "{next physical include}").unwrap();
            let e = call.context.path_entry_for(&p).unwrap();
            let trace = Trace {
                owner: &observer,
                samples: Default::default(),
            };
            let result = call.load_include_candidate(&p, &e, None, &trace);
            let peak = trace
                .samples
                .borrow()
                .iter()
                .map(|s| s.retained_bytes)
                .max()
                .unwrap();
            {
                let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
                if refusal {
                    assert!(
                        matches!(&result, Err(error) if matches!(error.error_ref(),SourceStoreError::RootLimit { .. })),
                        "expected actual post-report preparation refusal: {result:?}"
                    );
                    assert_eq!(cache.loaded.len(), 3);
                    assert_eq!(cache.loaded.capacity(), 3);
                    assert_eq!(cache.include_seen.len(), 3);
                    assert_eq!(cache.include_seen.capacity(), 3);
                    assert_eq!((call.include_files, call.include_bytes), counters);
                }
                for (id, loaded, seen) in keys {
                    assert_eq!(
                        cache
                            .loaded
                            .get_key_value(&id)
                            .unwrap()
                            .0
                            .0
                            .as_str()
                            .as_ptr(),
                        loaded
                    );
                    assert_eq!(
                        cache.include_seen.get(&id).unwrap().0.as_str().as_ptr(),
                        seen
                    );
                    assert_eq!(
                        cache.loaded.get(&id).unwrap().bytes.as_ptr(),
                        held.iter().find(|s| s.id == id).unwrap().bytes.as_ptr()
                    );
                }
            }
            if !refusal {
                let source = result.unwrap();
                assert_eq!(source.bytes.as_ref(), b"{next physical include}");
                assert_eq!(call.include_files, counters.0 + 1);
                assert_eq!(call.include_bytes, counters.1 + source.bytes.len());
                held.push(source);
            }
            assert!(
                call.report
                    .observations
                    .iter()
                    .any(|o| matches!(o,ResolutionObservation::Payload {path,..} if path==&p)),
                "truthful completed read report remains after either receiving preparation refuses"
            );
            assert!(budget.snapshot().work > before.work);
            assert!(budget.snapshot().byte_work > before.byte_work);
            let spent = budget.snapshot();
            drop(call);
            drop(resolver);
            drop(root);
            drop(budget);
            assert_eq!(held[0].bytes.as_ref(), b"{prior 0}");
            drop(held);
            assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
            assert_eq!(observer.budget_snapshot().work, spent.work);
            assert_eq!(observer.budget_snapshot().byte_work, spent.byte_work);
            (spent.work, peak)
        }
        let (work, peak) = run(CompileTimeLimits::default(), false);
        // Full three-entry receivers: seen preparation spends 2 hash + 3
        // sizing + 4 construction units after loaded preparation completes.
        run(
            CompileTimeLimits {
                max_work: work - 10,
                ..Default::default()
            },
            true,
        );
        run(
            CompileTimeLimits {
                max_work: work - 1,
                ..Default::default()
            },
            true,
        );
        run(
            CompileTimeLimits {
                max_retained_bytes: peak - 1,
                ..Default::default()
            },
            true,
        );
    }

    #[test]
    fn include_publication_actual_reuse_cancel_unwind_limits_and_retry_are_nonmutating() {
        use crate::compile_time::CompileTimeLimits;
        struct Poll {
            calls: std::sync::atomic::AtomicUsize,
            stop: usize,
            panic: bool,
        }
        impl CancellationToken for Poll {
            fn is_cancelled(&self) -> bool {
                let stopped = self.calls.fetch_add(1, Ordering::Relaxed) >= self.stop;
                if stopped && self.panic {
                    panic!("actual reused include helper unwind")
                }
                stopped
            }
        }
        for panic in [false, true] {
            let mut completed = false;
            let mut refused = 0;
            for stop in 0..500 {
                let (dir, _, mut resolver) = physical_report_resolver();
                let p = dir.path().join("value.inc");
                let e = resolver.context.path_entry_for(&p).unwrap();
                let mut budget =
                    CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
                let mut call = resolver.store_call(&mut budget).unwrap();
                let original = call
                    .load_include_candidate(&p, &e, None, &NoCancellation)
                    .unwrap();
                let root = call.compile_time_store.as_ref().unwrap().clone();
                let keys = {
                    let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
                    (
                        cache
                            .loaded
                            .get_key_value(&original.id)
                            .unwrap()
                            .0
                            .0
                            .as_str()
                            .as_ptr(),
                        cache
                            .include_seen
                            .get(&original.id)
                            .unwrap()
                            .0
                            .as_str()
                            .as_ptr(),
                    )
                };
                let counters = (call.include_files, call.include_bytes);
                let before = budget.snapshot();
                fs::write(&p, "{disk changes do not invalidate same-request snapshot}").unwrap();
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    call.load_include_candidate(
                        &p,
                        &e,
                        None,
                        &Poll {
                            calls: 0.into(),
                            stop,
                            panic,
                        },
                    )
                }));
                match outcome {
                    Err(_) => {
                        assert!(panic);
                        refused += 1;
                    }
                    Ok(Err(error)) if matches!(error.error_ref(), SourceStoreError::Cancelled) => {
                        assert!(!panic);
                        refused += 1;
                    }
                    Ok(Ok(source)) => {
                        assert_eq!(source.bytes.as_ptr(), original.bytes.as_ptr());
                        assert_eq!(source.revision, original.revision);
                        completed = true;
                    }
                    Ok(Err(error)) => panic!("unexpected reuse error: {error:?}"),
                }
                {
                    let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
                    assert_eq!(cache.loaded.len(), 1);
                    assert_eq!(cache.loaded.capacity(), 3);
                    assert_eq!(cache.include_seen.len(), 1);
                    assert_eq!(cache.include_seen.capacity(), 3);
                    assert_eq!(
                        cache
                            .loaded
                            .get_key_value(&original.id)
                            .unwrap()
                            .0
                            .0
                            .as_str()
                            .as_ptr(),
                        keys.0
                    );
                    assert_eq!(
                        cache
                            .include_seen
                            .get(&original.id)
                            .unwrap()
                            .0
                            .as_str()
                            .as_ptr(),
                        keys.1
                    );
                    assert_eq!(
                        cache.loaded.get(&original.id).unwrap().revision,
                        original.revision
                    );
                }
                assert_eq!((call.include_files, call.include_bytes), counters);
                let retry = call
                    .load_include_candidate(&p, &e, None, &NoCancellation)
                    .unwrap();
                assert_eq!(retry.bytes.as_ptr(), original.bytes.as_ptr());
                drop(retry);
                assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
                assert!(budget.snapshot().work > before.work);
                assert!(budget.snapshot().byte_work > before.byte_work);
                if completed {
                    break;
                }
            }
            assert!(completed);
            assert!(refused > 10);
        }
        let (dir, _, mut resolver) = physical_report_resolver();
        let p = dir.path().join("value.inc");
        let e = resolver.context.path_entry_for(&p).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut call = resolver.store_call(&mut budget).unwrap();
        let original = call
            .load_include_candidate(&p, &e, None, &NoCancellation)
            .unwrap();
        let root = call.compile_time_store.as_ref().unwrap().clone();
        let counters = (call.include_files, call.include_bytes);
        let next = dir.path().join("Next.inc");
        fs::write(&next, "{next exceeds remaining byte allowance}").unwrap();
        let entry = call.context.path_entry_for(&next).unwrap();
        call.limits.max_include_files = 1;
        assert!(matches!(
            call.load_include_candidate(&next, &entry, None, &NoCancellation),
            Err(error) if matches!(error.error_ref(),SourceStoreError::Incomplete { .. })
        ));
        call.limits.max_include_files = 2;
        call.limits.max_include_bytes = counters.1 + 1;
        assert!(matches!(
            call.load_include_candidate(&next, &entry, None, &NoCancellation),
            Err(error) if matches!(error.error_ref(),SourceStoreError::TooLarge { .. })
        ));
        assert_eq!((call.include_files, call.include_bytes), counters);
        let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(cache.loaded.len(), 1);
        assert_eq!(cache.include_seen.len(), 1);
        assert_eq!(
            cache.loaded.get(&original.id).unwrap().bytes.as_ptr(),
            original.bytes.as_ptr()
        );
    }

    #[test]
    fn include_publication_original_preparations_foreign_refusal_retry_and_equal_replacement() {
        use crate::compile_time::CompileTimeLimits;
        let (dir, _, mut resolver) = physical_report_resolver();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut call = resolver.store_call(&mut budget).unwrap();
        let mut prior = Vec::new();
        for i in 0..3 {
            let p = dir.path().join(format!("Prior{i}.inc"));
            fs::write(&p, format!("{{prior {i}}}")).unwrap();
            let e = call.context.path_entry_for(&p).unwrap();
            prior.push(
                call.load_include_candidate(&p, &e, None, &NoCancellation)
                    .unwrap(),
            );
        }
        let root = call.compile_time_store.as_ref().unwrap().clone();
        let counters = (call.include_files, call.include_bytes);
        let p = dir.path().join("Next.inc");
        fs::write(&p, "{next original preparation}").unwrap();
        let e = call.context.path_entry_for(&p).unwrap();
        let prepared = call
            .prepare_candidate_with_limit(&p, &e, None, SourceKind::Include, 4096, &NoCancellation)
            .unwrap();
        let (key, value) = prepared.cached.unwrap();
        let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
        let loaded = cache.prepare_loaded(key, value, &mut budget).unwrap();
        assert_eq!(cache.loaded.len(), 3);
        assert_eq!(cache.include_seen.len(), 3);
        assert!(loaded.growth.is_some());
        let mut foreign = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let query = include_query(&p, &mut foreign).unwrap();
        let before = foreign.snapshot();
        assert!(matches!(
            cache.prepare_seen(OwnedLoadedKey(query.id), &mut foreign),
            Err(SourceStoreError::AdmissionUnavailable)
        ));
        assert_eq!(foreign.snapshot().work, before.work);
        assert_eq!(cache.loaded.capacity(), 3);
        assert_eq!(cache.include_seen.capacity(), 3);
        assert_eq!((call.include_files, call.include_bytes), counters);
        drop(loaded);
        drop(cache);
        drop(prepared.loaded);
        drop(query.canonical);
        drop(query.identity);
        let before = foreign.snapshot();
        assert!(matches!(
            call.load_include_candidate_owned(&p, &e, None, &NoCancellation, &mut foreign),
            Err(error) if matches!(error.error_ref(),SourceStoreError::AdmissionUnavailable)
        ));
        assert_eq!(foreign.snapshot(), before);
        assert!(
            call.report
                .observations
                .iter()
                .any(|o| matches!(o,ResolutionObservation::Payload {path,..} if path==&p))
        );
        // Actual filesystem source preparation and actual empty receiving maps
        // are the helper's original constructors, not synthetic credit objects.
        let prepared = call
            .prepare_candidate_with_limit(&p, &e, None, SourceKind::Include, 4096, &NoCancellation)
            .unwrap();
        let (key, value) = prepared.cached.unwrap();
        let key_pointer = key.0.as_str().as_ptr();
        let bytes_pointer = value.bytes.as_ptr();
        let query = include_query(&p, &mut budget).unwrap();
        let seen_pointer = query.id.as_str().as_ptr();
        let mut cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
        let loaded = cache.prepare_loaded(key, value, &mut budget).unwrap();
        let seen = cache
            .prepare_seen(OwnedLoadedKey(query.id), &mut budget)
            .unwrap()
            .unwrap();
        assert!(loaded.growth.as_ref().unwrap().receiving.capacity() >= 4);
        assert!(seen.growth.as_ref().unwrap().receiving.capacity() >= 4);
        assert_eq!(cache.loaded.len(), 3);
        assert_eq!(cache.include_seen.len(), 3);
        let committed_counters = (
            counters.0.checked_add(1).unwrap(),
            counters.1.checked_add(prepared.loaded.bytes.len()).unwrap(),
        );
        budget.charge(0, 0, 0).unwrap();
        cache.value_mut().commit_loaded(loaded);
        cache.value_mut().commit_seen(seen);
        call.include_files = committed_counters.0;
        call.include_bytes = committed_counters.1;
        assert_eq!(
            cache
                .loaded
                .get_key_value(&prepared.loaded.id)
                .unwrap()
                .0
                .0
                .as_str()
                .as_ptr(),
            key_pointer
        );
        assert_eq!(
            cache
                .loaded
                .get(&prepared.loaded.id)
                .unwrap()
                .bytes
                .as_ptr(),
            bytes_pointer
        );
        assert_eq!(
            cache
                .include_seen
                .get(&prepared.loaded.id)
                .unwrap()
                .0
                .as_str()
                .as_ptr(),
            seen_pointer
        );
        let capacities = (cache.loaded.capacity(), cache.include_seen.capacity());
        drop(cache);
        drop(query.canonical);
        drop(query.identity);
        assert_eq!(prepared.loaded.bytes.as_ptr(), bytes_pointer);
        // Equal-ID replacement is the receiving primitive used by explicit
        // selected-source reloads; normal helper reuse must not refresh snapshots.
        fs::write(&p, "{revised selected source}").unwrap();
        let revised = call
            .prepare_candidate_with_limit(&p, &e, None, SourceKind::Include, 4096, &NoCancellation)
            .unwrap();
        assert_eq!(revised.loaded.id, prepared.loaded.id);
        assert_ne!(revised.loaded.revision, prepared.loaded.revision);
        let (key, value) = revised.cached.unwrap();
        let revised_key = key.0.as_str().as_ptr();
        let revised_bytes = value.bytes.as_ptr();
        let query = include_query(&p, &mut budget).unwrap();
        let mut cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
        let loaded = cache.prepare_loaded(key, value, &mut budget).unwrap();
        assert!(loaded.growth.is_none());
        assert!(
            cache
                .prepare_seen(OwnedLoadedKey(query.id), &mut budget)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            cache.loaded.get(&revised.loaded.id).unwrap().bytes.as_ptr(),
            bytes_pointer
        );
        budget.charge(0, 0, 0).unwrap();
        cache.value_mut().commit_loaded(loaded);
        assert_eq!(
            cache
                .loaded
                .get_key_value(&revised.loaded.id)
                .unwrap()
                .0
                .0
                .as_str()
                .as_ptr(),
            revised_key
        );
        assert_eq!(
            cache.loaded.get(&revised.loaded.id).unwrap().bytes.as_ptr(),
            revised_bytes
        );
        assert_eq!(
            cache
                .include_seen
                .get(&revised.loaded.id)
                .unwrap()
                .0
                .as_str()
                .as_ptr(),
            seen_pointer
        );
        assert_eq!(
            (cache.loaded.capacity(), cache.include_seen.capacity()),
            capacities
        );
        drop(cache);
        assert_eq!(prepared.loaded.bytes.as_ptr(), bytes_pointer);
        assert_eq!((call.include_files, call.include_bytes), committed_counters);
        let reused = call
            .load_include_candidate(&p, &e, None, &NoCancellation)
            .unwrap();
        assert_eq!(reused.bytes.as_ptr(), revised_bytes);
        assert_eq!(reused.revision, revised.loaded.revision);
        assert_eq!((call.include_files, call.include_bytes), committed_counters);
        assert_eq!(prior[0].bytes.as_ref(), b"{prior 0}");
    }

    #[test]
    fn include_reuse_owner_original_query_transfer_growth_removal_and_shared_lifetime() {
        use crate::compile_time::CompileTimeLimits;
        let (dir, _, mut resolver) = physical_report_resolver();
        let path = dir.path().join("value.inc");
        let entry = resolver.context.path_entry_for(&path).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let observer = budget
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut budget)
            .unwrap();
        let baseline = budget.snapshot().retained_bytes;
        let mut call = resolver.store_call(&mut budget).unwrap();
        let source = call
            .load_candidate_with_limit(
                &path,
                &entry,
                None,
                SourceKind::Include,
                4096,
                &NoCancellation,
            )
            .unwrap();
        let root = call.compile_time_store.as_ref().unwrap().clone();
        let before = budget.snapshot();
        let query = include_query(&dir.path().join("absent/../value.inc"), &mut budget).unwrap();
        assert_eq!(*query.canonical, path);
        assert_eq!(*query.id, source.id);
        assert_eq!(*query.identity, path_key(&path));
        let pointer = query.id.as_str().as_ptr();
        let key = OwnedLoadedKey(query.id);
        drop(query.canonical);
        drop(query.identity);
        assert!(budget.snapshot().retained_bytes > before.retained_bytes);
        {
            let mut cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
            assert!(cache.insert_seen(key, &mut budget).unwrap());
            assert_eq!(
                cache
                    .include_seen
                    .get(&source.id)
                    .unwrap()
                    .0
                    .as_str()
                    .as_ptr(),
                pointer
            );
        }
        let mut held = vec![source];
        let mut growths = 0;
        for i in 0..12 {
            let p = dir.path().join(format!("Seen{i}.inc"));
            fs::write(&p, format!("{{include {i}}}")).unwrap();
            let e = call.context.path_entry_for(&p).unwrap();
            let (cap, keys) = {
                let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
                (
                    cache.include_seen.capacity(),
                    cache
                        .include_seen
                        .iter()
                        .map(|k| ((*k.0).clone(), k.0.as_str().as_ptr()))
                        .collect::<Vec<_>>(),
                )
            };
            let next = call
                .load_include_candidate(&p, &e, None, &NoCancellation)
                .unwrap();
            {
                let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
                if cache.include_seen.capacity() > cap {
                    growths += 1;
                }
                for (id, pointer) in keys {
                    assert_eq!(
                        cache.include_seen.get(&id).unwrap().0.as_str().as_ptr(),
                        pointer
                    );
                }
                assert!(
                    cache
                        .include_seen_capacity
                        .as_ref()
                        .unwrap()
                        .is_admitted_by(&budget)
                );
            }
            held.push(next);
        }
        assert!(growths >= 2);
        {
            let mut cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
            let cap = cache.include_seen.capacity();
            let before = budget.snapshot();
            drop(cache.value_mut().include_seen.take(&held[1].id));
            assert_eq!(cache.include_seen.capacity(), cap);
            assert_eq!(
                before.retained_bytes - budget.snapshot().retained_bytes,
                held[1].id.as_str().len() + std::mem::size_of::<AdmittedOwned<SourceId>>()
            );
            assert_eq!(
                cache
                    .include_seen
                    .get(&held[0].id)
                    .unwrap()
                    .0
                    .as_str()
                    .as_ptr(),
                pointer
            );
        }
        drop(call);
        drop(resolver);
        drop(budget);
        assert!(
            root.view
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .include_seen
                .contains(&held[0].id)
        );
        assert_eq!(held[0].bytes.as_ref(), b"{physical include}");
        let spent = observer.budget_snapshot();
        drop(held);
        assert!(observer.budget_snapshot().retained_bytes > baseline);
        drop(root);
        assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
        assert_eq!(observer.budget_snapshot().work, spent.work);
        assert_eq!(observer.budget_snapshot().byte_work, spent.byte_work);
    }

    #[test]
    fn include_reuse_owner_actual_precopy_peak_refusal_foreign_root_and_set_capacity() {
        use crate::compile_time::CompileTimeLimits;
        let (dir, _, mut resolver) = physical_report_resolver();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut call = resolver.store_call(&mut budget).unwrap();
        let mut held = Vec::new();
        for i in 0..3 {
            let p = dir.path().join(format!("Full{i}.inc"));
            fs::write(&p, "{full seen set}").unwrap();
            let e = call.context.path_entry_for(&p).unwrap();
            held.push(
                call.load_include_candidate(&p, &e, None, &NoCancellation)
                    .unwrap(),
            );
        }
        let root = call.compile_time_store.as_ref().unwrap().clone();
        let p = dir.path().join("Peak.inc");
        fs::write(&p, "{peak include}").unwrap();
        let e = call.context.path_entry_for(&p).unwrap();
        let source = call
            .load_candidate_with_limit(&p, &e, None, SourceKind::Include, 4096, &NoCancellation)
            .unwrap();
        let query = include_query(&p, &mut budget).unwrap();
        let key = OwnedLoadedKey(query.id);
        drop(query.canonical);
        drop(query.identity);
        let live = budget.snapshot();
        let slot = std::mem::size_of::<OwnedLoadedKey>() + 1;
        budget
            .constrain_retained(live.retained_bytes + 4 * slot)
            .unwrap();
        {
            let mut cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(cache.include_seen.len(), 3);
            assert_eq!(cache.include_seen.capacity(), 3);
            assert!(matches!(
                cache.insert_seen(key, &mut budget),
                Err(SourceStoreError::RootLimit { .. })
            ));
            assert_eq!(cache.include_seen.len(), 3);
            assert_eq!(cache.include_seen.capacity(), 3);
            assert!(!cache.include_seen.contains(&source.id));
            assert!(
                cache
                    .include_seen_capacity
                    .as_ref()
                    .unwrap()
                    .is_admitted_by(&budget)
            );
        }
        assert!(budget.snapshot().work > live.work);
        budget
            .constrain_retained(budget.snapshot().retained_bytes)
            .unwrap();
        let before = budget.snapshot();
        assert!(matches!(
            include_query(&p, &mut budget),
            Err(SourceStoreError::RootLimit { .. })
        ));
        assert_eq!(budget.snapshot(), before);
        let mut foreign = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let query = include_query(&p, &mut foreign).unwrap();
        let before = foreign.snapshot();
        {
            let mut cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
            assert!(matches!(
                cache.insert_seen(OwnedLoadedKey(query.id), &mut foreign),
                Err(SourceStoreError::AdmissionUnavailable)
            ));
            assert_eq!(cache.include_seen.len(), 3);
        }
        assert_eq!(foreign.snapshot().work, before.work);
        assert_eq!(foreign.snapshot().byte_work, before.byte_work);
        drop(query.canonical);
        drop(query.identity);
        // Removal destroys only original keys. Real backing remains admitted
        // until it dies; this explicit teardown is test-only, not eviction.
        {
            let mut cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
            cache.value_mut().include_seen.clear();
            assert_eq!(cache.include_seen.capacity(), 3);
            let before = budget.snapshot();
            drop(std::mem::take(&mut cache.value_mut().include_seen));
            drop(cache.value_mut().include_seen_capacity.take());
            assert_eq!(
                before.retained_bytes - budget.snapshot().retained_bytes,
                4 * slot + 16 + std::mem::size_of::<AdmittedOwned<()>>()
            );
        }
        assert_eq!(held[0].bytes.as_ref(), b"{full seen set}");
        assert_eq!(source.bytes.as_ref(), b"{peak include}");
    }

    #[test]
    fn include_reuse_owner_case_legacy_route_snapshot_and_nonutf8_query_identity() {
        use crate::compile_time::CompileTimeLimits;
        path_identity::with_case_insensitive_volumes(|| {
            let (dir, root, mut resolver) = physical_report_resolver();
            let path = dir.path().join("MiXeD.inc");
            fs::write(&path, "{original mixed snapshot}").unwrap();
            let entry = resolver.context.path_entry_for(&path).unwrap();
            let route = LegacyRoute {
                source_path: root,
                sibling_directory: dir.path().into(),
            };
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let mut call = resolver.store_call(&mut budget).unwrap();
            let original = call
                .load_candidate_with_limit(
                    &path,
                    &entry,
                    Some(&route),
                    SourceKind::Include,
                    4096,
                    &NoCancellation,
                )
                .unwrap();
            let requested = dir.path().join("absent/../mixed.inc");
            let reused = call
                .load_include_candidate(&requested, &entry, Some(&route), &NoCancellation)
                .unwrap();
            assert_eq!(reused.id, original.id);
            assert_eq!(reused.bytes.as_ptr(), original.bytes.as_ptr());
            fs::write(&path, "{changed disk snapshot stays unused}").unwrap();
            let before = budget.snapshot();
            let repeated = call
                .load_include_candidate(&requested, &entry, Some(&route), &NoCancellation)
                .unwrap();
            assert_eq!(repeated.revision, original.revision);
            assert_eq!(repeated.bytes.as_ptr(), original.bytes.as_ptr());
            drop(repeated);
            assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
            let root = call.compile_time_store.as_ref().unwrap().clone();
            assert!(
                root.view
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .include_seen
                    .contains(&original.id)
            );
            assert!(!call.report.observations.iter().any(|o|matches!(o,ResolutionObservation::Payload {revision,..} if revision!=&original.revision)));
        });
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let (dir, _, mut resolver) = physical_report_resolver();
            let path = dir
                .path()
                .join(std::ffi::OsString::from_vec(b"Bad\xff.inc".to_vec()));
            fs::write(&path, "{nonutf8 include}").unwrap();
            let entry = resolver.context.path_entry_for(&path).unwrap();
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let query = include_query(&path, &mut budget).unwrap();
            assert_eq!(*query.id, source_id_for_path(&path));
            assert_eq!(*query.identity, path_key(&path));
            drop(query);
            let source = resolver
                .store_call(&mut budget)
                .unwrap()
                .load_include_candidate(&path, &entry, None, &NoCancellation)
                .unwrap();
            assert_eq!(source.path, path);
            assert_eq!(source.bytes.as_ref(), b"{nonutf8 include}");
        }
    }

    #[test]
    fn include_reuse_owner_actual_query_seen_growth_cancel_unwind_and_errors_keep_prior_owners() {
        use crate::compile_time::CompileTimeLimits;
        struct Poll {
            calls: std::sync::atomic::AtomicUsize,
            stop: usize,
            panic: bool,
        }
        impl CancellationToken for Poll {
            fn is_cancelled(&self) -> bool {
                let stopped = self.calls.fetch_add(1, Ordering::Relaxed) >= self.stop;
                if stopped && self.panic {
                    panic!("actual include query/reuse/seen unwind")
                }
                stopped
            }
        }
        for panic in [false, true] {
            let mut completed = false;
            let mut failures = 0;
            for stop in 0..1000 {
                let (dir, _, mut resolver) = physical_report_resolver();
                let mut budget =
                    CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
                let observer = budget
                    .admit_owned(0, 0, 0, || ())
                    .unwrap()
                    .into_shared(&mut budget)
                    .unwrap();
                let baseline = budget.snapshot().retained_bytes;
                let mut call = resolver.store_call(&mut budget).unwrap();
                let mut held = Vec::new();
                for i in 0..3 {
                    let p = dir.path().join(format!("Prior{i}.inc"));
                    fs::write(&p, format!("{{prior {i}}}")).unwrap();
                    let e = call.context.path_entry_for(&p).unwrap();
                    held.push(
                        call.load_include_candidate(&p, &e, None, &NoCancellation)
                            .unwrap(),
                    );
                }
                let root = call.compile_time_store.as_ref().unwrap().clone();
                let pointers = {
                    let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
                    assert_eq!(cache.include_seen.len(), cache.include_seen.capacity());
                    cache
                        .include_seen
                        .iter()
                        .map(|k| ((*k.0).clone(), k.0.as_str().as_ptr()))
                        .collect::<Vec<_>>()
                };
                let p = dir.path().join("Next.inc");
                fs::write(&p, "{next physical include}").unwrap();
                let e = call.context.path_entry_for(&p).unwrap();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    call.load_include_candidate(
                        &p,
                        &e,
                        None,
                        &Poll {
                            calls: 0.into(),
                            stop,
                            panic,
                        },
                    )
                }));
                let failure = match result {
                    Err(_) => {
                        assert!(panic);
                        failures += 1;
                        true
                    }
                    Ok(Err(error)) if matches!(error.error_ref(), SourceStoreError::Cancelled) => {
                        assert!(!panic);
                        failures += 1;
                        true
                    }
                    Ok(Ok(source)) => {
                        held.push(source);
                        completed = true;
                        false
                    }
                    Ok(Err(error)) => panic!("unexpected {stop}: {error:?}"),
                };
                {
                    let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
                    for (id, pointer) in pointers {
                        assert_eq!(
                            cache.include_seen.get(&id).unwrap().0.as_str().as_ptr(),
                            pointer
                        );
                        assert_eq!(
                            cache.loaded.get(&id).unwrap().bytes.as_ptr(),
                            held.iter().find(|s| s.id == id).unwrap().bytes.as_ptr()
                        );
                    }
                    if failure {
                        assert_eq!(cache.include_seen.len(), 3);
                        assert_eq!(cache.include_seen.capacity(), 3);
                        assert_eq!(
                            cache.loaded.len(),
                            3,
                            "failed include helper must not publish loaded pair at poll {stop}, panic={panic}"
                        );
                        assert_eq!(
                            cache.loaded.capacity(),
                            3,
                            "failed helper preserves original loaded backing"
                        );
                        assert_eq!(call.include_files, 3);
                        assert_eq!(call.include_bytes, b"{prior 0}".len() * 3);
                    } else {
                        assert_eq!(cache.include_seen.len(), 4);
                        assert!(cache.include_seen.capacity() > 3);
                    }
                }
                if failure {
                    let retry = call
                        .load_include_candidate(&p, &e, None, &NoCancellation)
                        .unwrap();
                    let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
                    assert_eq!(
                        cache.loaded.get(&retry.id).unwrap().bytes.as_ptr(),
                        retry.bytes.as_ptr()
                    );
                    assert_eq!(cache.loaded.len(), 4);
                    assert_eq!(cache.include_seen.len(), 4);
                    assert_eq!(call.include_files, 4);
                    assert_eq!(
                        call.include_bytes,
                        b"{prior 0}".len() * 3 + retry.bytes.len()
                    );
                    drop(cache);
                    held.push(retry);
                }
                let spent = budget.snapshot();
                drop(call);
                drop(resolver);
                drop(root);
                drop(budget);
                assert_eq!(held[0].bytes.as_ref(), b"{prior 0}");
                drop(held);
                assert_eq!(
                    observer.budget_snapshot().retained_bytes,
                    baseline,
                    "actual last owner retirement at poll {stop}, panic={panic}"
                );
                assert_eq!(observer.budget_snapshot().work, spent.work);
                assert_eq!(observer.budget_snapshot().byte_work, spent.byte_work);
                if completed {
                    break;
                }
            }
            assert!(completed);
            assert!(failures > 20);
        }
        let (dir, _, mut resolver) = physical_report_resolver();
        let p = dir.path().join("value.inc");
        let e = resolver.context.path_entry_for(&p).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut call = resolver.store_call(&mut budget).unwrap();
        let original = call
            .load_include_candidate(&p, &e, None, &NoCancellation)
            .unwrap();
        let root = call.compile_time_store.as_ref().unwrap().clone();
        let missing = dir.path().join("Missing.inc");
        let denied = dir.path().join("Directory.inc");
        fs::create_dir(&denied).unwrap();
        for path in [&missing, &denied] {
            let entry = call.context.path_entry_for(path).unwrap();
            assert!(matches!(
                call.load_include_candidate(path, &entry, None, &NoCancellation),
                Err(error) if matches!(error.error_ref(),SourceStoreError::NotFound { .. } | SourceStoreError::Unauthorized { .. })
            ));
            assert!(
                root.view
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .include_seen
                    .contains(&original.id)
            );
        }
        call.limits.max_include_files = 1;
        let next = dir.path().join("Limit.inc");
        fs::write(&next, "{limited}").unwrap();
        let e = call.context.path_entry_for(&next).unwrap();
        assert!(matches!(
            call.load_include_candidate(&next, &e, None, &NoCancellation),
            Err(error) if matches!(error.error_ref(),SourceStoreError::Incomplete { .. })
        ));
        assert_eq!(
            root.view
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .include_seen
                .len(),
            1
        );
        assert_eq!(original.bytes.as_ref(), b"{physical include}");
    }

    #[test]
    fn loaded_cache_owner_actual_include_revision_reuse_and_replacement_preserve_reports() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        let (dir, root, mut resolver) = physical_report_resolver();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let request = || IncludeResolveRequest {
            including_path: &root,
            byte_range: 0..0,
            requested_name: "value.inc",
            legacy_route: None,
        };
        let first = resolver
            .include_capability(request(), &mut budget, &NoCancellation)
            .unwrap();
        let Resolution::Found(old) = first.result() else {
            panic!("actual authorized include")
        };
        let old_pointer = old.bytes.as_ptr();
        let cache_root = resolver
            .compile_time_caches
            .iter()
            .find(|r| r.view.is_admitted_by(&budget))
            .unwrap()
            .clone();
        let (key_pointer, capacity) = {
            let cache = cache_root.view.lock().unwrap_or_else(|e| e.into_inner());
            (
                cache
                    .loaded
                    .get_key_value(&old.id)
                    .unwrap()
                    .0
                    .0
                    .as_str()
                    .as_ptr(),
                cache.loaded.capacity(),
            )
        };
        let reused = resolver
            .include_capability(request(), &mut budget, &NoCancellation)
            .unwrap();
        assert!(
            matches!(reused.result(),Resolution::Found(s) if s.bytes.as_ptr()==old_pointer && s.revision==old.revision)
        );
        {
            let cache = cache_root.view.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(
                cache
                    .loaded
                    .get_key_value(&old.id)
                    .unwrap()
                    .0
                    .0
                    .as_str()
                    .as_ptr(),
                key_pointer
            );
        }
        fs::write(
            dir.path().join("value.inc"),
            "{changed physical include version}",
        )
        .unwrap();
        let same_request = resolver
            .include_capability(request(), &mut budget, &NoCancellation)
            .unwrap();
        assert!(
            matches!(same_request.result(),Resolution::Found(s) if s.revision==old.revision && s.bytes.as_ptr()==old_pointer),
            "preserve existing authorization-only same-request include reuse"
        );
        let path = dir.path().join("value.inc");
        let entry = resolver.context.path_entry_for(&path).unwrap();
        let new = resolver
            .store_call(&mut budget)
            .unwrap()
            .load_candidate_with_limit(
                &path,
                &entry,
                None,
                SourceKind::Include,
                4096,
                &NoCancellation,
            )
            .unwrap();
        assert_eq!(old.id, new.id);
        assert_ne!(old.revision, new.revision);
        assert_ne!(new.bytes.as_ptr(), old_pointer);
        {
            let cache = cache_root.view.lock().unwrap_or_else(|e| e.into_inner());
            let (key, value) = cache.loaded.get_key_value(&new.id).unwrap();
            assert_ne!(key.0.as_str().as_ptr(), key_pointer);
            assert_eq!(value.bytes.as_ptr(), new.bytes.as_ptr());
            assert_eq!(cache.loaded.len(), 1);
            assert_eq!(cache.loaded.capacity(), capacity);
        }
        assert!(first.reports().observations().iter().any(
            |o| matches!(o,ResolutionObservation::Payload {revision,..} if revision==&old.revision)
        ));
        assert!(resolver.report.observations.iter().any(
            |o| matches!(o,ResolutionObservation::Payload {revision,..} if revision==&new.revision)
        ));
        let changed = resolver
            .include_capability(request(), &mut budget, &NoCancellation)
            .unwrap();
        assert!(
            matches!(changed.result(),Resolution::Found(s) if s.revision==new.revision && s.bytes.as_ptr()==new.bytes.as_ptr())
        );
        drop(resolver);
        drop(cache_root);
        drop(budget);
        assert_eq!(old.bytes.as_ptr(), old_pointer);
        assert_eq!(new.bytes.as_ref(), b"{changed physical include version}");
    }

    #[test]
    fn loaded_cache_owner_actual_versions_growth_removal_and_last_root_lifetime() {
        use crate::compile_time::CompileTimeLimits;
        let (dir, _, mut resolver) = physical_report_resolver();
        let path = dir.path().join("Other.pas");
        let entry = resolver.context.path_entry_for(&path).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let observer = budget
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut budget)
            .unwrap();
        let baseline = budget.snapshot().retained_bytes;
        let mut call = resolver.store_call(&mut budget).unwrap();
        let old = call
            .load_candidate_with_limit(&path, &entry, None, SourceKind::Unit, 4096, &NoCancellation)
            .unwrap();
        let old_pointer = old.bytes.as_ptr();
        let root = call.compile_time_store.as_ref().unwrap().clone();
        let old_key_pointer = {
            let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
            let (key, value) = cache.loaded.get_key_value(&old.id).unwrap();
            assert_eq!(value.bytes.as_ptr(), old_pointer);
            assert!(key.0.is_admitted_by(&budget));
            key.0.as_str().as_ptr()
        };
        fs::write(
            &path,
            "unit Other; interface {new version} implementation end.",
        )
        .unwrap();
        let new = call
            .load_candidate_with_limit(&path, &entry, None, SourceKind::Unit, 4096, &NoCancellation)
            .unwrap();
        assert_eq!(old.id, new.id);
        assert_ne!(old.revision, new.revision);
        assert_eq!(old.bytes.as_ptr(), old_pointer);
        assert_ne!(new.bytes.as_ptr(), old_pointer);
        {
            let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
            let (key, value) = cache.loaded.get_key_value(&new.id).unwrap();
            assert_ne!(
                key.0.as_str().as_ptr(),
                old_key_pointer,
                "new original clone replaces equal old key"
            );
            assert_eq!(value.bytes.as_ptr(), new.bytes.as_ptr());
        }
        let mut held = vec![old, new];
        let mut growths = 0;
        for i in 0..12 {
            let p = dir.path().join(format!("Grow{i}.pas"));
            fs::write(&p, format!("unit Grow{i}; interface implementation end.")).unwrap();
            let e = call.context.path_entry_for(&p).unwrap();
            let (cap, keys) = {
                let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
                (
                    cache.loaded.capacity(),
                    cache
                        .loaded
                        .keys()
                        .map(|k| ((*k.0).clone(), k.0.as_str().as_ptr()))
                        .collect::<Vec<_>>(),
                )
            };
            let source = call
                .load_candidate_with_limit(&p, &e, None, SourceKind::Unit, 4096, &NoCancellation)
                .unwrap();
            {
                let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
                if cache.loaded.capacity() > cap {
                    growths += 1;
                }
                for (id, pointer) in keys {
                    assert_eq!(
                        cache
                            .loaded
                            .get_key_value(&id)
                            .unwrap()
                            .0
                            .0
                            .as_str()
                            .as_ptr(),
                        pointer,
                        "rehash moves original key owners without second clones"
                    );
                }
                assert_eq!(
                    cache.loaded.get(&source.id).unwrap().bytes.as_ptr(),
                    source.bytes.as_ptr()
                );
                assert!(
                    cache
                        .loaded_capacity
                        .as_ref()
                        .unwrap()
                        .is_admitted_by(&budget)
                );
            }
            held.push(source);
        }
        assert!(growths >= 2);
        let before = budget.snapshot();
        {
            let mut cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
            let cap = cache.loaded.capacity();
            let receipt = cache
                .loaded_capacity
                .as_ref()
                .unwrap()
                .is_admitted_by(&budget);
            let removed = cache.value_mut().loaded.remove_entry(&held[1].id).unwrap();
            drop(removed);
            assert_eq!(cache.loaded.capacity(), cap);
            assert_eq!(
                cache
                    .loaded_capacity
                    .as_ref()
                    .unwrap()
                    .is_admitted_by(&budget),
                receipt
            );
        }
        assert!(budget.snapshot().retained_bytes < before.retained_bytes);
        assert_eq!(held[0].bytes.as_ptr(), old_pointer);
        assert!(call.report.observations.iter().any(|o|matches!(o,ResolutionObservation::Payload {revision,..} if revision==&held[0].revision)));
        {
            let mut cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
            let cap = cache.loaded.capacity();
            drop(cache.value_mut().loaded.drain().collect::<Vec<_>>());
            assert_eq!(
                cache.loaded.capacity(),
                cap,
                "entry death does not destroy receiving backing"
            );
            let live = budget.snapshot();
            let expected = (cap + 1).next_power_of_two()
                * (std::mem::size_of::<(OwnedLoadedKey, LoadedSource)>() + 1)
                + 16
                + std::mem::size_of::<AdmittedOwned<()>>();
            assert!(live.retained_bytes >= expected);
            // Test-only teardown follows actual backing death before credit
            // death; production never clears/evicts this root to gain budget.
            drop(std::mem::take(&mut cache.value_mut().loaded));
            drop(cache.value_mut().loaded_capacity.take());
            assert_eq!(
                live.retained_bytes - budget.snapshot().retained_bytes,
                expected,
                "only this independent actual map invoice retires"
            );
        }
        drop(call);
        drop(resolver);
        drop(budget);
        assert_eq!(held[0].bytes.as_ptr(), old_pointer);
        let spent = observer.budget_snapshot();
        drop(held);
        assert!(observer.budget_snapshot().retained_bytes > baseline);
        drop(root);
        assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
        assert_eq!(observer.budget_snapshot().work, spent.work);
        assert_eq!(observer.budget_snapshot().byte_work, spent.byte_work);
    }

    #[test]
    fn loaded_cache_owner_original_prepared_pair_peak_refusal_and_foreign_root() {
        use crate::compile_time::CompileTimeLimits;
        let (dir, _, mut resolver) = physical_report_resolver();
        let path = dir.path().join("Other.pas");
        let entry = resolver.context.path_entry_for(&path).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut call = resolver.store_call(&mut budget).unwrap();
        let source = call
            .load_candidate_with_limit(&path, &entry, None, SourceKind::Unit, 4096, &NoCancellation)
            .unwrap();
        let root = call.compile_time_store.as_ref().unwrap().clone();
        // Consume an actual independently prepared original pair into its real
        // receiving cache. No extra source/key copies occur at insertion.
        let cached = source.admitted_copy(&mut budget).unwrap();
        let pointer = cached.bytes.as_ptr();
        let key = loaded_cache_key(&source.id, &mut budget).unwrap();
        let key_pointer = key.0.as_str().as_ptr();
        {
            let mut cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
            cache.insert_loaded(key, cached, &mut budget).unwrap();
            let (key, value) = cache.loaded.get_key_value(&source.id).unwrap();
            assert_eq!(key.0.as_str().as_ptr(), key_pointer);
            assert_eq!(value.bytes.as_ptr(), pointer);
        }
        // Fill the actual production map to its retained backing capacity.
        let mut i = 0;
        loop {
            let full = {
                let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
                cache.loaded.len() == cache.loaded.capacity()
            };
            if full {
                break;
            }
            let p = dir.path().join(format!("Fill{i}.pas"));
            fs::write(&p, format!("unit Fill{i}; interface implementation end.")).unwrap();
            let e = call.context.path_entry_for(&p).unwrap();
            drop(
                call.load_candidate_with_limit(
                    &p,
                    &e,
                    None,
                    SourceKind::Unit,
                    4096,
                    &NoCancellation,
                )
                .unwrap(),
            );
            i += 1;
        }
        // Read a real new authorized source before testing the exact receiving
        // construction boundary, with the original cache already full.
        let p = dir.path().join("Peak.pas");
        fs::write(&p, "unit Peak; interface implementation end.").unwrap();
        let e = call.context.path_entry_for(&p).unwrap();
        let fresh = call
            .resolver
            .store
            .load_admitted(
                SourceRequest {
                    path: &p,
                    entry: &e,
                    read_policy: &call.resolver.context.read_policy,
                    legacy_route: None,
                    kind: SourceKind::Unit,
                    max_bytes: 4096,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
        let cached = fresh.admitted_copy(&mut budget).unwrap();
        let key = loaded_cache_key(&fresh.id, &mut budget).unwrap();
        let mut cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
        let capacity = cache.loaded.capacity();
        let len = cache.loaded.len();
        let old_capacity = cache
            .loaded_capacity
            .as_ref()
            .unwrap()
            .is_admitted_by(&budget);
        let live = budget.snapshot();
        let slot = std::mem::size_of::<(OwnedLoadedKey, LoadedSource)>() + 1;
        let old_bytes = (capacity + 1).next_power_of_two() * slot
            + 16
            + std::mem::size_of::<AdmittedOwned<()>>();
        let new_bytes = (len + 1).next_power_of_two() * 2 * slot
            + 16
            + std::mem::size_of::<AdmittedOwned<()>>();
        budget
            .constrain_retained(live.retained_bytes + new_bytes - old_bytes)
            .unwrap();
        assert!(matches!(
            cache.insert_loaded(key, cached, &mut budget),
            Err(SourceStoreError::RootLimit { .. })
        ));
        assert_eq!(cache.loaded.len(), len);
        assert_eq!(cache.loaded.capacity(), capacity);
        assert_eq!(
            cache
                .loaded_capacity
                .as_ref()
                .unwrap()
                .is_admitted_by(&budget),
            old_capacity
        );
        assert!(!cache.loaded.contains_key(&fresh.id));
        assert_eq!(
            cache
                .loaded
                .get_key_value(&source.id)
                .unwrap()
                .0
                .0
                .as_str()
                .as_ptr(),
            key_pointer
        );
        assert!(
            budget.snapshot().work > live.work,
            "performed sizing/hash work survives receiving refusal"
        );
        drop(cache);
        budget
            .constrain_retained(budget.snapshot().retained_bytes)
            .unwrap();
        let before = budget.snapshot();
        assert!(matches!(
            loaded_cache_key(&fresh.id, &mut budget),
            Err(SourceStoreError::RootLimit { .. })
        ));
        assert_eq!(
            budget.snapshot(),
            before,
            "refused original key clone does not run"
        );
        let mut foreign = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let foreign_key = loaded_cache_key(&source.id, &mut foreign).unwrap();
        let before = foreign.snapshot();
        let mut cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
        assert!(matches!(
            cache.insert_loaded(foreign_key, fresh, &mut foreign),
            Err(SourceStoreError::AdmissionUnavailable)
        ));
        assert_eq!(cache.loaded.len(), len);
        assert_eq!(cache.loaded.capacity(), capacity);
        assert_eq!(foreign.snapshot().work, before.work);
        assert_eq!(
            cache.loaded.get(&source.id).unwrap().bytes.as_ptr(),
            source.bytes.as_ptr()
        );
    }

    #[test]
    fn loaded_cache_owner_actual_growth_cancel_unwind_and_store_errors_keep_prior_pairs() {
        use crate::compile_time::CompileTimeLimits;
        struct Poll {
            calls: std::sync::atomic::AtomicUsize,
            stop: usize,
            panic: bool,
        }
        impl CancellationToken for Poll {
            fn is_cancelled(&self) -> bool {
                let stop = self.calls.fetch_add(1, Ordering::Relaxed) >= self.stop;
                if stop && self.panic {
                    panic!("actual loaded cache construction/growth/report unwind")
                }
                stop
            }
        }
        for panic in [false, true] {
            let mut completed = false;
            let mut refused = 0;
            for stop in 0..700 {
                let (dir, _, mut resolver) = physical_report_resolver();
                let mut budget =
                    CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
                let observer = budget
                    .admit_owned(0, 0, 0, || ())
                    .unwrap()
                    .into_shared(&mut budget)
                    .unwrap();
                let baseline = budget.snapshot().retained_bytes;
                let mut call = resolver.store_call(&mut budget).unwrap();
                let mut held = Vec::new();
                for i in 0..3 {
                    let p = dir.path().join(format!("Prior{i}.pas"));
                    fs::write(&p, format!("unit Prior{i}; interface implementation end.")).unwrap();
                    let e = call.context.path_entry_for(&p).unwrap();
                    held.push(
                        call.load_candidate_with_limit(
                            &p,
                            &e,
                            None,
                            SourceKind::Unit,
                            4096,
                            &NoCancellation,
                        )
                        .unwrap(),
                    );
                }
                let root = call.compile_time_store.as_ref().unwrap().clone();
                let (cap, keys) = {
                    let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
                    assert_eq!(cache.loaded.len(), cache.loaded.capacity());
                    (
                        cache.loaded.capacity(),
                        cache
                            .loaded
                            .keys()
                            .map(|k| ((*k.0).clone(), k.0.as_str().as_ptr()))
                            .collect::<Vec<_>>(),
                    )
                };
                let p = dir.path().join("Next.pas");
                fs::write(&p, "unit Next; interface implementation end.").unwrap();
                let e = call.context.path_entry_for(&p).unwrap();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    call.load_candidate_with_limit(
                        &p,
                        &e,
                        None,
                        SourceKind::Unit,
                        4096,
                        &Poll {
                            calls: 0.into(),
                            stop,
                            panic,
                        },
                    )
                }));
                let failure = match result {
                    Err(_) => {
                        assert!(panic);
                        refused += 1;
                        true
                    }
                    Ok(Err(SourceStoreError::Cancelled)) => {
                        assert!(!panic);
                        refused += 1;
                        true
                    }
                    Ok(Ok(source)) => {
                        held.push(source);
                        completed = true;
                        false
                    }
                    Ok(Err(error)) => panic!("unexpected {stop}: {error:?}"),
                };
                {
                    let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
                    if failure {
                        assert_eq!(
                            cache.loaded.len(),
                            3,
                            "failed leaf must not publish new loaded pair at poll {stop}, panic={panic}"
                        );
                        assert_eq!(
                            cache.loaded.capacity(),
                            cap,
                            "failed leaf must preserve original backing"
                        );
                    }
                    for (id, pointer) in keys {
                        let (key, value) = cache.loaded.get_key_value(&id).unwrap();
                        assert_eq!(key.0.as_str().as_ptr(), pointer);
                        assert_eq!(
                            value.bytes.as_ptr(),
                            held.iter().find(|s| s.id == id).unwrap().bytes.as_ptr()
                        );
                    }
                    if cache.loaded.len() == 3 {
                        assert_eq!(cache.loaded.capacity(), cap);
                    } else {
                        assert_eq!(cache.loaded.len(), 4);
                        assert!(cache.loaded.capacity() > cap);
                    }
                }
                let spent = budget.snapshot();
                drop(call);
                drop(resolver);
                drop(budget);
                assert!(held[0].bytes.starts_with(b"unit Prior0"));
                drop(root);
                assert!(
                    observer.budget_snapshot().retained_bytes > baseline,
                    "external admitted source owners outlive cache"
                );
                drop(held);
                assert_eq!(
                    observer.budget_snapshot().retained_bytes,
                    baseline,
                    "last original storage owner dies at poll {stop}, panic={panic}"
                );
                assert_eq!(observer.budget_snapshot().work, spent.work);
                assert_eq!(observer.budget_snapshot().byte_work, spent.byte_work);
                if completed {
                    break;
                }
            }
            assert!(completed);
            assert!(refused > 20);
        }
        let (dir, _, mut resolver) = physical_report_resolver();
        let p = dir.path().join("Other.pas");
        let e = resolver.context.path_entry_for(&p).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut call = resolver.store_call(&mut budget).unwrap();
        let source = call
            .load_candidate_with_limit(&p, &e, None, SourceKind::Unit, 4096, &NoCancellation)
            .unwrap();
        let root = call.compile_time_store.as_ref().unwrap().clone();
        let pointer = {
            let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
            cache
                .loaded
                .get_key_value(&source.id)
                .unwrap()
                .0
                .0
                .as_str()
                .as_ptr()
        };
        let missing = dir.path().join("Missing.pas");
        let denied = dir.path().join("Directory.pas");
        fs::create_dir(&denied).unwrap();
        for (path, max) in [(&missing, 4096), (&denied, 4096), (&p, 1)] {
            let e = call.context.path_entry_for(path).unwrap();
            assert!(matches!(
                call.load_candidate_with_limit(
                    path,
                    &e,
                    None,
                    SourceKind::Unit,
                    max,
                    &NoCancellation
                ),
                Err(SourceStoreError::NotFound { .. }
                    | SourceStoreError::Unauthorized { .. }
                    | SourceStoreError::TooLarge { .. })
            ));
            let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(cache.loaded.len(), 1);
            assert_eq!(
                cache
                    .loaded
                    .get_key_value(&source.id)
                    .unwrap()
                    .0
                    .0
                    .as_str()
                    .as_ptr(),
                pointer
            );
            assert_eq!(
                cache.loaded.get(&source.id).unwrap().bytes.as_ptr(),
                source.bytes.as_ptr()
            );
        }
    }

    #[test]
    fn loaded_cache_owner_empty_receiving_root_rejects_other_roots_original_pair() {
        use crate::compile_time::CompileTimeLimits;
        let (_dir, _, mut receiver) = physical_report_resolver();
        let mut receiving_budget =
            CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let call = receiver.store_call(&mut receiving_budget).unwrap();
        let receiving = call.compile_time_store.as_ref().unwrap().clone();
        let (dir, _, mut foreign_resolver) = physical_report_resolver();
        let path = dir.path().join("Other.pas");
        let entry = foreign_resolver.context.path_entry_for(&path).unwrap();
        let mut foreign = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let source = foreign_resolver
            .store_call(&mut foreign)
            .unwrap()
            .load_candidate_with_limit(&path, &entry, None, SourceKind::Unit, 4096, &NoCancellation)
            .unwrap();
        let cached = source.admitted_copy(&mut foreign).unwrap();
        let key = loaded_cache_key(&source.id, &mut foreign).unwrap();
        let original = receiving_budget.snapshot();
        let before = foreign.snapshot();
        let mut cache = receiving.view.lock().unwrap_or_else(|e| e.into_inner());
        assert!(cache.loaded_capacity.is_none());
        assert!(matches!(
            cache.insert_loaded(key, cached, &mut foreign),
            Err(SourceStoreError::AdmissionUnavailable)
        ));
        assert!(cache.loaded.is_empty());
        assert_eq!(cache.loaded.capacity(), 0);
        assert!(cache.loaded_capacity.is_none());
        assert_eq!(receiving_budget.snapshot(), original);
        assert_eq!(foreign.snapshot().work, before.work);
        assert_eq!(foreign.snapshot().byte_work, before.byte_work);
        assert!(source.bytes.starts_with(b"unit Other"));
    }

    #[test]
    fn loaded_publication_replacement_every_leaf_unwind_cancel_preserves_prior_then_retries() {
        use crate::compile_time::CompileTimeLimits;
        struct Poll {
            calls: std::sync::atomic::AtomicUsize,
            stop: usize,
            panic: bool,
        }
        impl CancellationToken for Poll {
            fn is_cancelled(&self) -> bool {
                let stopped = self.calls.fetch_add(1, Ordering::Relaxed) >= self.stop;
                if stopped && self.panic {
                    panic!("replacement leaf report/cache unwind")
                }
                stopped
            }
        }
        for panic in [true, false] {
            let mut finished = false;
            let mut refused = 0;
            let mut failure_after_report = 0;
            for stop in 0..700 {
                let (dir, _, mut resolver) = physical_report_resolver();
                let path = dir.path().join("Other.pas");
                let entry = resolver.context.path_entry_for(&path).unwrap();
                let mut budget =
                    CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
                let observer = budget
                    .admit_owned(0, 0, 0, || ())
                    .unwrap()
                    .into_shared(&mut budget)
                    .unwrap();
                let baseline = budget.snapshot().retained_bytes;
                let mut call = resolver.store_call(&mut budget).unwrap();
                let old = call
                    .load_candidate_with_limit(
                        &path,
                        &entry,
                        None,
                        SourceKind::Unit,
                        4096,
                        &NoCancellation,
                    )
                    .unwrap();
                let root = call.compile_time_store.as_ref().unwrap().clone();
                let (key_pointer, cap) = {
                    let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
                    (
                        cache
                            .loaded
                            .get_key_value(&old.id)
                            .unwrap()
                            .0
                            .0
                            .as_str()
                            .as_ptr(),
                        cache.loaded.capacity(),
                    )
                };
                fs::write(
                    &path,
                    "unit Other; interface {changed physical version} implementation end.",
                )
                .unwrap();
                let before = budget.snapshot();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    call.load_candidate_with_limit(
                        &path,
                        &entry,
                        None,
                        SourceKind::Unit,
                        4096,
                        &Poll {
                            calls: 0.into(),
                            stop,
                            panic,
                        },
                    )
                }));
                let loaded = match result {
                    Err(_) => {
                        assert!(panic);
                        refused += 1;
                        None
                    }
                    Ok(Err(SourceStoreError::Cancelled)) => {
                        assert!(!panic);
                        refused += 1;
                        None
                    }
                    Ok(Ok(source)) => {
                        finished = true;
                        Some(source)
                    }
                    Ok(Err(error)) => panic!("unexpected leaf {stop}: {error:?}"),
                };
                if loaded.is_none() {
                    let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
                    assert_eq!(cache.loaded.len(), 1);
                    assert_eq!(cache.loaded.capacity(), cap);
                    let (key, value) = cache.loaded.get_key_value(&old.id).unwrap();
                    assert_eq!(
                        key.0.as_str().as_ptr(),
                        key_pointer,
                        "failed replacement leaf must preserve original key at poll {stop}, panic={panic}"
                    );
                    assert_eq!(value.bytes.as_ptr(), old.bytes.as_ptr());
                    assert_eq!(
                        value.revision, old.revision,
                        "failed replacement leaf must not publish revised source"
                    );
                    drop(cache);
                    if call.report.observations.iter().any(|o|matches!(o,ResolutionObservation::Payload {revision,..} if revision!=&old.revision)) {failure_after_report+=1;}
                }
                assert!(budget.snapshot().work >= before.work);
                assert!(budget.snapshot().byte_work >= before.byte_work);
                let new = loaded.unwrap_or_else(|| {
                    call.load_candidate_with_limit(
                        &path,
                        &entry,
                        None,
                        SourceKind::Unit,
                        4096,
                        &NoCancellation,
                    )
                    .unwrap()
                });
                assert_ne!(new.revision, old.revision);
                assert_ne!(new.bytes.as_ptr(), old.bytes.as_ptr());
                {
                    let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
                    let (key, value) = cache.loaded.get_key_value(&new.id).unwrap();
                    assert_ne!(key.0.as_str().as_ptr(), key_pointer);
                    assert_eq!(value.bytes.as_ptr(), new.bytes.as_ptr());
                    assert_eq!(value.revision, new.revision);
                    assert_eq!(cache.loaded.capacity(), cap);
                }
                assert!(call.report.observations.iter().any(|o|matches!(o,ResolutionObservation::Payload {revision,..} if revision==&new.revision)));
                let spent = budget.snapshot();
                drop(call);
                drop(resolver);
                drop(root);
                drop(budget);
                assert!(old.bytes.starts_with(b"unit Other"));
                assert!(
                    new.bytes
                        .windows(b"changed physical version".len())
                        .any(|w| w == b"changed physical version")
                );
                drop(old);
                drop(new);
                assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
                assert_eq!(observer.budget_snapshot().work, spent.work);
                assert_eq!(observer.budget_snapshot().byte_work, spent.byte_work);
                if finished {
                    break;
                }
            }
            assert!(finished);
            assert!(refused > 20);
            assert!(
                failure_after_report > 0,
                "sweep reaches completed report then precommit failure, without revised cache pair"
            );
        }
    }

    #[test]
    fn loaded_publication_actual_report_prepared_work_and_peak_refusals_do_not_commit() {
        use crate::compile_time::{BudgetSnapshot, CompileTimeLimits};
        struct Trace<'a> {
            owner: &'a AdmittedShared<()>,
            samples: std::cell::RefCell<Vec<BudgetSnapshot>>,
        }
        impl CancellationToken for Trace<'_> {
            fn is_cancelled(&self) -> bool {
                self.samples.borrow_mut().push(self.owner.budget_snapshot());
                false
            }
        }
        fn run(
            growth: bool,
            limits: CompileTimeLimits,
            refusal: bool,
            report_ready: bool,
        ) -> (usize, usize) {
            let (dir, _, mut resolver) = physical_report_resolver();
            let mut budget = CompileTimeBudget::new(limits, &NoCancellation);
            let observer = budget
                .admit_owned(0, 0, 0, || ())
                .unwrap()
                .into_shared(&mut budget)
                .unwrap();
            let baseline = budget.snapshot().retained_bytes;
            let mut call = resolver.store_call(&mut budget).unwrap();
            let mut held = Vec::new();
            for i in 0..3 {
                let path = dir.path().join(format!("Prior{i}.pas"));
                fs::write(
                    &path,
                    format!("unit Prior{i}; interface implementation end."),
                )
                .unwrap();
                let entry = call.context.path_entry_for(&path).unwrap();
                held.push(
                    call.load_candidate_with_limit(
                        &path,
                        &entry,
                        None,
                        SourceKind::Unit,
                        4096,
                        &NoCancellation,
                    )
                    .unwrap(),
                );
            }
            let root = call.compile_time_store.as_ref().unwrap().clone();
            let keys = {
                let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
                assert_eq!(cache.loaded.len(), cache.loaded.capacity());
                cache
                    .loaded
                    .keys()
                    .map(|k| ((*k.0).clone(), k.0.as_str().as_ptr()))
                    .collect::<Vec<_>>()
            };
            let path = dir
                .path()
                .join(if growth { "Next.pas" } else { "Prior0.pas" });
            fs::write(
                &path,
                "unit NewVersion; interface {changed version} implementation end.",
            )
            .unwrap();
            let entry = call.context.path_entry_for(&path).unwrap();
            let before = budget.snapshot();
            let trace = Trace {
                owner: &observer,
                samples: Default::default(),
            };
            let result =
                call.load_candidate_with_limit(&path, &entry, None, SourceKind::Unit, 4096, &trace);
            let last = *trace.samples.borrow().last().unwrap();
            let receiving = if growth {
                8 * (std::mem::size_of::<(OwnedLoadedKey, LoadedSource)>() + 1)
                    + 16
                    + std::mem::size_of::<AdmittedOwned<()>>()
            } else {
                0
            };
            let peak = (last.retained_bytes + receiving).max(
                trace
                    .samples
                    .borrow()
                    .iter()
                    .map(|s| s.retained_bytes)
                    .max()
                    .unwrap(),
            );
            if refusal {
                assert!(
                    matches!(result, Err(SourceStoreError::RootLimit { .. })),
                    "actual report-prepared receiving/work refusal: {result:?}"
                );
                let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
                assert_eq!(cache.loaded.len(), 3);
                assert_eq!(cache.loaded.capacity(), 3);
                for (id, pointer) in keys {
                    let (key, value) = cache.loaded.get_key_value(&id).unwrap();
                    assert_eq!(key.0.as_str().as_ptr(), pointer);
                    let old = held.iter().find(|s| s.id == id).unwrap();
                    assert_eq!(value.bytes.as_ptr(), old.bytes.as_ptr());
                    assert_eq!(value.revision, old.revision);
                }
            } else {
                let source = result.unwrap();
                assert!(
                    source
                        .bytes
                        .windows(b"changed version".len())
                        .any(|w| w == b"changed version")
                );
                let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
                assert_eq!(
                    cache.loaded.get(&source.id).unwrap().bytes.as_ptr(),
                    source.bytes.as_ptr()
                );
                drop(cache);
                held.push(source);
            }
            assert_eq!(call.report.observations.iter().any(|o|matches!(o,ResolutionObservation::Payload {path:p,revision:SourceRevision::Disk {stamp,..},..} if p==&path && stamp.bytes==b"unit NewVersion; interface {changed version} implementation end.".len() as u64)),report_ready,"completed read evidence survives cache refusal; refused report copy does not fabricate a report");
            assert!(budget.snapshot().work > before.work);
            assert!(budget.snapshot().byte_work > before.byte_work);
            let spent = budget.snapshot();
            drop(call);
            drop(resolver);
            drop(root);
            drop(budget);
            drop(held);
            assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
            assert_eq!(observer.budget_snapshot().work, spent.work);
            assert_eq!(observer.budget_snapshot().byte_work, spent.byte_work);
            (spent.work, peak)
        }
        for growth in [false, true] {
            let (work, peak) = run(growth, CompileTimeLimits::default(), false, true);
            run(
                growth,
                CompileTimeLimits {
                    max_work: work - 1,
                    ..Default::default()
                },
                true,
                true,
            );
            if !growth {
                // Replacement's final cache charge is three work units; the
                // immediately preceding payload-copy admission is one unit.
                run(
                    false,
                    CompileTimeLimits {
                        max_work: work - 4,
                        ..Default::default()
                    },
                    true,
                    false,
                );
            }
            if growth {
                run(
                    growth,
                    CompileTimeLimits {
                        max_retained_bytes: peak - 1,
                        ..Default::default()
                    },
                    true,
                    true,
                );
            }
        }
    }

    #[test]
    fn source_load_scratch_actual_leaf_cancel_unwind_error_and_foreign_owner_preserve_prior_source()
    {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        struct Poll {
            calls: std::sync::atomic::AtomicUsize,
            stop: usize,
            panic: bool,
        }
        impl CancellationToken for Poll {
            fn is_cancelled(&self) -> bool {
                let stopped = self.calls.fetch_add(1, Ordering::Relaxed) >= self.stop;
                if stopped && self.panic {
                    panic!("actual source request/read/report unwind")
                }
                stopped
            }
        }
        for panic in [false, true] {
            let mut completed = false;
            let mut refused = 0;
            for stop in 0..700 {
                let (dir, root, mut resolver) = physical_report_resolver();
                let query = dir.path().join("absent/../Other.pas");
                let entry = resolver
                    .context
                    .path_entry_for(&dir.path().join("Other.pas"))
                    .unwrap();
                let mut budget =
                    CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
                let observer = budget
                    .admit_owned(0, 0, 0, || ())
                    .unwrap()
                    .into_shared(&mut budget)
                    .unwrap();
                let baseline = budget.snapshot().retained_bytes;
                let prior = resolver
                    .include_capability(
                        IncludeResolveRequest {
                            including_path: &root,
                            byte_range: 0..0,
                            requested_name: "value.inc",
                            legacy_route: None,
                        },
                        &mut budget,
                        &NoCancellation,
                    )
                    .unwrap();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut call = resolver.store_call(&mut budget).unwrap();
                    call.load_candidate_with_limit(
                        &query,
                        &entry,
                        None,
                        SourceKind::Unit,
                        4096,
                        &Poll {
                            calls: 0.into(),
                            stop,
                            panic,
                        },
                    )
                }));
                match result {
                    Err(_) => {
                        assert!(panic);
                        refused += 1;
                    }
                    Ok(Err(SourceStoreError::Cancelled)) => {
                        assert!(!panic);
                        refused += 1;
                    }
                    Ok(Ok(source)) => {
                        assert!(source.path.ends_with("Other.pas"));
                        assert!(source.compile_time_backing.is_some());
                        drop(source);
                        completed = true;
                    }
                    Ok(Err(error)) => panic!("unexpected source error at {stop}: {error:?}"),
                }
                let spent = budget.snapshot();
                drop(resolver);
                drop(budget);
                assert!(
                    matches!(prior.result(),Resolution::Found(source) if source.bytes.as_ref()==b"{physical include}")
                );
                assert!(!prior.reports().observations().is_empty());
                drop(prior);
                assert_eq!(
                    observer.budget_snapshot().retained_bytes,
                    baseline,
                    "scratch/cache/report/source drop at {stop}, panic={panic}"
                );
                assert_eq!(observer.budget_snapshot().work, spent.work);
                assert_eq!(observer.budget_snapshot().byte_work, spent.byte_work);
                if completed {
                    break;
                }
            }
            assert!(completed);
            assert!(refused > 20);
        }
        let (dir, root, mut resolver) = physical_report_resolver();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let prior = resolver
            .include_capability(
                IncludeResolveRequest {
                    including_path: &root,
                    byte_range: 0..0,
                    requested_name: "value.inc",
                    legacy_route: None,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
        let bad = dir.path().join("Bad.pas");
        fs::create_dir(&bad).unwrap();
        let entry = resolver.context.path_entry_for(&bad).unwrap();
        let mut call = resolver.store_call(&mut budget).unwrap();
        assert!(matches!(
            call.load_candidate_with_limit(
                &bad,
                &entry,
                None,
                SourceKind::Unit,
                4096,
                &NoCancellation
            ),
            Err(SourceStoreError::Unauthorized { .. })
        ));
        let good = dir.path().join("Other.pas");
        assert!(matches!(
            call.load_candidate_with_limit(
                &good,
                &entry,
                None,
                SourceKind::Unit,
                1,
                &NoCancellation
            ),
            Err(SourceStoreError::TooLarge { .. })
        ));
        let (path, local) = call
            .source_request_scratch(&good, &entry, &NoCancellation)
            .unwrap();
        let LookupScratch::Admitted(path) = path else {
            panic!("actual original path owner")
        };
        let mut foreign = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let before = foreign.snapshot();
        assert!(path.map_value(&mut foreign, |path| path).is_err());
        assert_eq!(foreign.snapshot(), before);
        assert_eq!(local.path, good);
        assert!(matches!(prior.result(), Resolution::Found(_)));
        assert!(!call.report.observations.is_empty());
    }

    #[test]
    fn source_load_scratch_original_path_and_mapped_entry_drop_independently() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        let (dir, root, mut resolver) = physical_report_resolver();
        let query = dir.path().join("absent/../value.inc");
        let mapped = ProjectPathEntry {
            path: query.clone(),
            provenance: ProjectPathProvenance::Mapped {
                root: dir.path().into(),
            },
        };
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let prior = resolver
            .include_capability(
                IncludeResolveRequest {
                    including_path: &root,
                    byte_range: 0..0,
                    requested_name: "value.inc",
                    legacy_route: None,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
        let before = budget.snapshot();
        let mut call = resolver.store_call(&mut budget).unwrap();
        let (path, entry) = call
            .source_request_scratch(&query, &mapped, &NoCancellation)
            .unwrap();
        let LookupScratch::Admitted(path) = path else {
            panic!("actual typed path constructor")
        };
        let LookupScratch::Admitted(entry) = entry else {
            panic!("actual typed local entry constructor")
        };
        assert_eq!(&*path, &canonical_path(&query));
        assert_eq!(entry.path, *path);
        assert_eq!(entry.provenance, mapped.provenance);
        assert_eq!(path.capacity(), query.as_os_str().len());
        assert_eq!(
            budget.snapshot().retained_bytes,
            before.retained_bytes
                + path.capacity()
                + std::mem::size_of::<AdmittedOwned<PathBuf>>()
                + entry.path.capacity()
                + dir.path().as_os_str().len()
                + std::mem::size_of::<AdmittedOwned<ProjectPathEntry>>()
        );
        let entry_pointer = entry.path.as_os_str().as_encoded_bytes().as_ptr();
        drop(path);
        assert_eq!(
            entry.path.as_os_str().as_encoded_bytes().as_ptr(),
            entry_pointer
        );
        drop(entry);
        assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
        let loaded = call
            .load_candidate_with_limit(
                &query,
                &mapped,
                None,
                SourceKind::Include,
                4096,
                &NoCancellation,
            )
            .unwrap();
        assert!(
            matches!(&loaded.revision,SourceRevision::Disk {path_entry,..} if path_entry.provenance==mapped.provenance)
        );
        assert!(call.report.observations.iter().any(|o|matches!(o,ResolutionObservation::Candidate {path,entry:Some(e),..} if path==&dir.path().join("value.inc") && e.provenance==mapped.provenance)));
        assert!(
            matches!(prior.result(),Resolution::Found(source) if source.path==dir.path().join("value.inc"))
        );
    }

    #[test]
    fn source_load_scratch_success_case_legacy_route_cache_report_and_backing_are_independent() {
        use crate::compile_time::CompileTimeLimits;
        let (dir, root, mut resolver) = physical_report_resolver();
        let file = dir.path().join("MiXeD.inc");
        fs::write(&file, "{case source payload}").unwrap();
        let query = dir.path().join("absent/../MiXeD.inc");
        let entry = ProjectPathEntry::legacy(query.clone());
        let route = LegacyRoute {
            source_path: root,
            sibling_directory: dir.path().into(),
        };
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let observer = budget
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut budget)
            .unwrap();
        let baseline = budget.snapshot().retained_bytes;
        let loaded = {
            let mut call = resolver.store_call(&mut budget).unwrap();
            let loaded = call
                .load_candidate_with_limit(
                    &query,
                    &entry,
                    Some(&route),
                    SourceKind::Include,
                    4096,
                    &NoCancellation,
                )
                .unwrap();
            assert_eq!(loaded.path, file);
            assert_eq!(loaded.bytes.as_ref(), b"{case source payload}");
            let SourceRevision::Disk { path_entry, .. } = &loaded.revision else {
                panic!("physical source revision")
            };
            assert_eq!(path_entry.path, file);
            assert_eq!(path_entry.provenance, ProjectPathProvenance::LegacyNative);
            assert!(call.report.observations.iter().any(|o|matches!(o,ResolutionObservation::Payload {path,source_id,..} if path==&file && source_id==&loaded.id)));
            let root = call.compile_time_store.as_ref().unwrap();
            let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(
                cache.loaded.get(&loaded.id).unwrap().bytes.as_ptr(),
                loaded.bytes.as_ptr()
            );
            drop(cache);
            loaded
        };
        let pointer = loaded.bytes.as_ptr();
        let backing = loaded.compile_time_backing.as_ref().unwrap().clone();
        let report = resolver.finish_admitted(&mut budget).unwrap();
        drop(budget);
        assert_eq!(loaded.bytes.as_ptr(), pointer);
        assert!(
            report
                .report()
                .observations
                .iter()
                .any(|o| matches!(o,ResolutionObservation::Payload {path,..} if path==&file))
        );
        drop(report);
        drop(loaded);
        assert_eq!(backing.bytes.as_ptr(), pointer);
        assert!(observer.budget_snapshot().retained_bytes > baseline);
        let spent = observer.budget_snapshot();
        drop(backing);
        assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
        assert_eq!(observer.budget_snapshot().work, spent.work);
        assert_eq!(observer.budget_snapshot().byte_work, spent.byte_work);
    }

    #[test]
    fn source_load_scratch_precopy_work_retained_refusal_and_policy_preflight_do_not_walk() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        struct Probe(std::cell::Cell<usize>);
        impl CancellationToken for Probe {
            fn is_cancelled(&self) -> bool {
                self.0.set(self.0.get() + 1);
                false
            }
        }
        let (dir, root, mut resolver) = physical_report_resolver();
        let query = dir.path().join("value.inc");
        let entry = resolver.context.path_entry_for(&query).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let prior = resolver
            .include_capability(
                IncludeResolveRequest {
                    including_path: &root,
                    byte_range: 0..0,
                    requested_name: "value.inc",
                    legacy_route: None,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
        let before = budget.snapshot();
        budget.constrain_retained(before.retained_bytes).unwrap();
        let probe = Probe(0.into());
        let call = resolver.store_call(&mut budget).unwrap();
        assert!(matches!(
            call.source_request_scratch(&query, &entry, &probe),
            Err(SourceStoreError::RootLimit { .. })
        ));
        assert_eq!(
            probe.0.get(),
            1,
            "admission cancellation poll occurs, but no constructor body poll/copy"
        );
        assert_eq!(budget.snapshot(), before);
        drop(call);
        probe.0.set(0);
        let limits = CompileTimeLimits {
            max_work: 0,
            ..Default::default()
        };
        let mut empty = CompileTimeBudget::new(limits, &NoCancellation);
        assert!(matches!(
            source_request_path(&query, &mut empty, &probe),
            Err(SourceStoreError::RootLimit { .. })
        ));
        assert_eq!(probe.0.get(), 0);
        assert_eq!(empty.snapshot().work, 0);
        let policy = &resolver.context.read_policy;
        let mut payload_visits = 0;
        let error = policy.visit_owned_clone_payload_with_preflight(&mut |work, bytes| {
            if bytes.is_none() {
                assert!(work > 0);
                return Err("preflight refusal".into());
            }
            payload_visits += 1;
            Ok(())
        });
        assert!(error.is_err());
        assert_eq!(payload_visits, 0);
        let mut plain = Vec::new();
        policy
            .visit_owned_clone_payload(&mut |bytes| {
                plain.push(bytes);
                Ok(())
            })
            .unwrap();
        let mut bounded = Vec::new();
        policy
            .visit_owned_clone_payload_with_preflight(&mut |_, bytes| {
                if let Some(bytes) = bytes {
                    bounded.push(bytes);
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(
            plain, bounded,
            "read-only preflight bridge preserves exact old clone payload/provenance visitor"
        );
        assert!(matches!(prior.result(), Resolution::Found(_)));
    }

    #[test]
    fn include_r18_actual_first_hit_cancel_every_poll_and_precopy_refusal_keep_prior_reason() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        struct Poll {
            calls: std::sync::atomic::AtomicUsize,
            stop: usize,
        }
        impl CancellationToken for Poll {
            fn is_cancelled(&self) -> bool {
                self.calls.fetch_add(1, Ordering::Relaxed) >= self.stop
            }
        }
        let mut done = false;
        let mut cancelled = 0;
        for stop in 0..500 {
            let (_dir, root, mut resolver) = physical_report_resolver();
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let observer = budget
                .admit_owned(0, 0, 0, || ())
                .unwrap()
                .into_shared(&mut budget)
                .unwrap();
            let baseline = budget.snapshot().retained_bytes;
            let prior = resolver
                .include_capability(
                    IncludeResolveRequest {
                        including_path: &root,
                        byte_range: 0..0,
                        requested_name: "absent.inc",
                        legacy_route: None,
                    },
                    &mut budget,
                    &NoCancellation,
                )
                .unwrap();
            match resolver.include_capability(
                IncludeResolveRequest {
                    including_path: &root,
                    byte_range: 0..0,
                    requested_name: "value.inc",
                    legacy_route: None,
                },
                &mut budget,
                &Poll {
                    calls: 0.into(),
                    stop,
                },
            ) {
                Err(ResolverError::Cancelled) => cancelled += 1,
                Ok(out) => {
                    assert!(
                        matches!(out.result(),Resolution::Found(source) if source.bytes.as_ref()==b"{physical include}")
                    );
                    drop(out);
                    done = true;
                }
                Err(other) => panic!("unexpected error at {stop}: {other:?}"),
            }
            let spent = budget.snapshot();
            drop(resolver);
            drop(budget);
            assert!(
                matches!(prior.result(),Resolution::Unavailable {reason} if reason.contains("absent.inc"))
            );
            drop(prior);
            assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
            assert_eq!(observer.budget_snapshot().work, spent.work);
            assert_eq!(observer.budget_snapshot().byte_work, spent.byte_work);
            if done {
                break;
            }
        }
        assert!(done);
        assert!(cancelled > 10);
        let (_dir, root, mut resolver) = physical_report_resolver();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let prior = resolver
            .include_capability(
                IncludeResolveRequest {
                    including_path: &root,
                    byte_range: 0..0,
                    requested_name: "absent.inc",
                    legacy_route: None,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
        let before = budget.snapshot();
        budget.constrain_retained(before.retained_bytes).unwrap();
        assert!(matches!(
            resolver.include_capability(
                IncludeResolveRequest {
                    including_path: &root,
                    byte_range: 0..0,
                    requested_name: "value.inc",
                    legacy_route: None
                },
                &mut budget,
                &NoCancellation
            ),
            Err(ResolverError::LimitExceeded { .. })
        ));
        assert!(matches!(prior.result(),Resolution::Unavailable {reason} if !reason.is_empty()));
        assert!(budget.snapshot().work >= before.work);
        assert!(budget.snapshot().byte_work >= before.byte_work);
    }

    #[test]
    fn include_reason_owner_real_root_and_provider_child_failure_consumers() {
        use crate::compile_time::{AdmittedProviderSource, CompileTimeLimits, CompileTimeResolver};
        struct Recording {
            inner: UnitResolver<FilesystemSourceStore>,
            reason: Option<usize>,
            from: Option<PathBuf>,
        }
        impl CompileTimeResolver for Recording {
            fn include_capability(
                &mut self,
                r: IncludeResolveRequest<'_>,
                b: &mut CompileTimeBudget<'_>,
                c: &dyn CancellationToken,
            ) -> Result<AdmittedCapabilityOutcome<LoadedSource>, ResolverError> {
                let from = r.including_path;
                let out = self.inner.include_capability(r, b, c)?;
                if let Resolution::Unavailable { reason } | Resolution::Incomplete { reason, .. } =
                    out.result()
                {
                    self.reason = Some(reason.as_ptr() as usize);
                    self.from = Some(from.to_path_buf());
                }
                Ok(out)
            }
            fn unit_report_capability(
                &mut self,
                r: UnitResolveRequest<'_>,
                b: &mut CompileTimeBudget<'_>,
                c: &dyn CancellationToken,
            ) -> Result<AdmittedCapabilityOutcome<AdmittedProviderSource>, ResolverError>
            {
                self.inner.unit_report_capability(r, b, c)
            }
            fn include(
                &mut self,
                r: IncludeResolveRequest<'_>,
                b: &mut CompileTimeBudget<'_>,
                c: &dyn CancellationToken,
            ) -> Result<ResolutionOutcome<LoadedSource>, ResolverError> {
                self.inner.include(r, b, c)
            }
            fn unit(
                &mut self,
                r: UnitResolveRequest<'_>,
                b: &mut CompileTimeBudget<'_>,
                c: &dyn CancellationToken,
            ) -> Result<ResolutionOutcome<crate::compile_time::ProviderSource>, ResolverError>
            {
                self.inner.unit(r, b, c)
            }
        }
        for scenario in 0..3 {
            let provider = scenario == 2;
            let (dir, path, mut inner) = physical_report_resolver();
            let text = if provider {
                "unit Main; interface uses Other; {$IF SizeOf(Other.T)=8}{$DEFINE HIT}{$ENDIF} implementation end."
            } else if scenario == 1 {
                "unit Main; interface {$I bad.inc} implementation end."
            } else {
                "unit Main; interface {$I absent.inc} implementation end."
            };
            fs::write(&path, text).unwrap();
            fs::create_dir(dir.path().join("bad.inc")).unwrap();
            fs::write(
                dir.path().join("Other.pas"),
                "unit Other; interface {$I absent.inc} type T=Int64; implementation end.",
            )
            .unwrap();
            inner.context.conditional_context = pascal_project::ConditionalContext::default()
                .with_compiler_version(pascal_project::CompilerVersion::new(37, 0))
                .with_layout(pascal_project::LayoutContext {
                    platform: Some(pascal_project::LayoutPlatform::Win32),
                    defaults: Default::default(),
                });
            inner.context.library_conditional_context = inner.context.conditional_context.clone();
            let context = inner.context.conditional_context.clone();
            let root = inner
                .load_source(&path, None, SourceKind::Unit, &NoCancellation)
                .unwrap();
            let mut recording = Recording {
                inner,
                reason: None,
                from: None,
            };
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let observer = budget
                .admit_owned(0, 0, 0, || ())
                .unwrap()
                .into_shared(&mut budget)
                .unwrap();
            let baseline = budget.snapshot().retained_bytes;
            let output = crate::compile_time::analyze_source_in_session(
                &root,
                &context,
                &mut recording,
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
            assert!(
                recording.reason.is_some(),
                "actual failure callback reached in root/provider child"
            );
            if provider {
                assert!(recording.from.as_ref().unwrap().ends_with("Other.pas"));
                assert!(
                    !output
                        .root
                        .directives
                        .iter()
                        .any(|d| d.body == "DEFINE HIT"
                            && d.activity == crate::conditional::Truth::True),
                    "failed provider child remains conservative"
                );
            } else {
                let reason = match &output.includes[0].target {
                    Resolution::Unavailable { reason } | Resolution::Incomplete { reason, .. } => {
                        reason
                    }
                    _ => panic!("missing include target"),
                };
                assert_eq!(
                    Some(reason.as_ptr() as usize),
                    recording.reason,
                    "original callback reason moves through IncludeOccurrence and final publication without cloning"
                );
            }
            drop(recording);
            drop(root);
            drop(budget);
            let live = observer.budget_snapshot();
            assert!(live.retained_bytes > baseline);
            if !provider {
                assert!(
                    matches!(&output.includes[0].target,Resolution::Unavailable {reason}|Resolution::Incomplete {reason,..} if reason.contains(if scenario==1 {"bad.inc"} else {"absent.inc"}))
                );
            }
            drop(output);
            assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
            assert_eq!(observer.budget_snapshot().work, live.work);
            assert_eq!(observer.budget_snapshot().byte_work, live.byte_work);
        }
    }

    #[test]
    fn include_limit_error_owner_real_provider_child_limits_keep_facts_conservative() {
        use crate::compile_time::{
            AdmittedProviderSource, CompileTimeLimits, CompileTimeResolver, Fact, OccurrenceId,
        };
        struct Recording {
            inner: UnitResolver<FilesystemSourceStore>,
            include_from: Option<PathBuf>,
            provider_request: Option<(String, PathBuf)>,
            warning: Option<(String, usize)>,
            incomplete: bool,
        }
        impl CompileTimeResolver for Recording {
            fn include_capability(
                &mut self,
                r: IncludeResolveRequest<'_>,
                b: &mut CompileTimeBudget<'_>,
                c: &dyn CancellationToken,
            ) -> Result<AdmittedCapabilityOutcome<LoadedSource>, ResolverError> {
                self.include_from = Some(r.including_path.to_path_buf());
                assert_eq!(r.requested_name, "export.inc");
                let out = self.inner.include_capability(r, b, c)?;
                self.incomplete = matches!(out.result(),Resolution::Incomplete {reason,candidates} if reason=="include export.inc lookup was incomplete" && candidates.is_empty());
                if self.incomplete {
                    let warning = out
                        .reports()
                        .warnings()
                        .iter()
                        .find(|w| w.starts_with("include candidate could not be loaded:"))
                        .expect("actual typed limit warning");
                    self.warning = Some((warning.clone(), warning.as_ptr() as usize));
                } else {
                    assert!(
                        matches!(out.result(),Resolution::Found(source) if source.path.ends_with("export.inc"))
                    );
                }
                Ok(out)
            }
            fn unit_report_capability(
                &mut self,
                r: UnitResolveRequest<'_>,
                b: &mut CompileTimeBudget<'_>,
                c: &dyn CancellationToken,
            ) -> Result<AdmittedCapabilityOutcome<AdmittedProviderSource>, ResolverError>
            {
                self.provider_request = Some((r.requested_name.into(), r.importer_path.into()));
                self.inner.unit_report_capability(r, b, c)
            }
            fn include(
                &mut self,
                r: IncludeResolveRequest<'_>,
                b: &mut CompileTimeBudget<'_>,
                c: &dyn CancellationToken,
            ) -> Result<ResolutionOutcome<LoadedSource>, ResolverError> {
                self.inner.include(r, b, c)
            }
            fn unit(
                &mut self,
                r: UnitResolveRequest<'_>,
                b: &mut CompileTimeBudget<'_>,
                c: &dyn CancellationToken,
            ) -> Result<ResolutionOutcome<crate::compile_time::ProviderSource>, ResolverError>
            {
                self.inner.unit(r, b, c)
            }
        }
        for limit in [None, Some("file"), Some("byte")] {
            let (dir, path, mut inner) = physical_report_resolver();
            fs::write(&path,"unit Main; interface uses Other; {$IF SizeOf(Other.Export)=8}{$DEFINE HIT}{$ENDIF} implementation end.").unwrap();
            let provider_path = dir.path().join("Other.pas");
            fs::write(
                &provider_path,
                "unit Other; interface {$I export.inc} implementation end.",
            )
            .unwrap();
            let include_path = dir.path().join("export.inc");
            fs::write(&include_path, "type Export=Int64;").unwrap();
            inner.context.conditional_context = pascal_project::ConditionalContext::default()
                .with_compiler_version(pascal_project::CompilerVersion::new(37, 0))
                .with_layout(pascal_project::LayoutContext {
                    platform: Some(pascal_project::LayoutPlatform::Win32),
                    defaults: Default::default(),
                });
            inner.context.library_conditional_context = inner.context.conditional_context.clone();
            let context = inner.context.conditional_context.clone();
            let root = inner
                .load_source(&path, None, SourceKind::Unit, &NoCancellation)
                .unwrap();
            if limit == Some("file") {
                // Limit the actual resolver helper, not provider-query admission.
                inner.limits.max_include_files = 0;
            }
            let expected_directory = inner
                .context
                .include_search_entries(&provider_path)
                .into_iter()
                .find(|entry| entry.path == dir.path())
                .expect("authorized provider include search provenance");
            if limit == Some("byte") {
                inner.limits.max_include_bytes = 0;
            }
            let mut recording = Recording {
                inner,
                include_from: None,
                provider_request: None,
                warning: None,
                incomplete: false,
            };
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let observer = budget
                .admit_owned(0, 0, 0, || ())
                .unwrap()
                .into_shared(&mut budget)
                .unwrap();
            let baseline = budget.snapshot().retained_bytes;
            let output = crate::compile_time::analyze_source_in_session(
                &root,
                &context,
                &mut recording,
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
            assert_eq!(
                recording.provider_request.as_ref(),
                Some(&("Other".into(), path.clone()))
            );
            assert_eq!(recording.include_from.as_ref(), Some(&provider_path));
            let activity = output
                .root
                .directives
                .iter()
                .find(|d| d.body == "DEFINE HIT")
                .expect("downstream conditional fact")
                .activity;
            assert_eq!(
                activity,
                if limit.is_some() {
                    crate::conditional::Truth::Unknown
                } else {
                    crate::conditional::Truth::True
                }
            );
            let site = output
                .facts
                .site_at(&root.id, OccurrenceId(0), root.bytes.len())
                .unwrap();
            // Public fact queries use a separate query budget, as in existing
            // provider tests; their manual scratch is not output-owner storage.
            let mut query_budget =
                CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let fact = output
                .facts
                .layout_name(site, "Other.Export", &mut query_budget);
            if limit.is_some() {
                assert!(
                    matches!(fact, Fact::Unknown(_)),
                    "failed provider child must expose unknown layout, got {fact:?}"
                );
            } else {
                assert!(
                    matches!(fact, Fact::Known(_)),
                    "unrestricted actual included declaration proves layout: {fact:?}"
                );
            }
            assert!(
                output.observations.iter().any(
                    |o| matches!(o,ResolutionObservation::Payload {path,..} if path==&provider_path)
                ),
                "actual provider was read"
            );
            let expected = limit.map(|kind| {
                format!(
                    "include candidate could not be loaded: {:?}",
                    SourceStoreError::Incomplete {
                        path: include_path.clone(),
                        reason: format!("include {kind} limit (0) reached")
                    }
                )
            });
            if let Some(expected) = &expected {
                assert!(recording.incomplete);
                assert_eq!(&recording.warning.as_ref().unwrap().0, expected);
                let warning = output
                    .warnings
                    .iter()
                    .find(|w| *w == expected)
                    .expect("transported provider-child warning");
                assert_eq!(
                    warning.as_ptr() as usize,
                    recording.warning.as_ref().unwrap().1,
                    "original admitted callback warning is consumed into actual output"
                );
                assert!(output.observations.iter().any(|o|matches!(o,ResolutionObservation::Directory {path,entry,complete:true,..} if path==dir.path() && entry==&expected_directory)),"failed-lookup search directory/provenance survives: {:?}",output.observations);
                assert!(!output.observations.iter().any(|o|matches!(o,ResolutionObservation::Payload {path,..} if path==&include_path)),"limit forbids include payload read");
            } else {
                assert!(!recording.incomplete);
                assert!(recording.warning.is_none());
                assert!(output.observations.iter().any(
                    |o| matches!(o,ResolutionObservation::Payload {path,..} if path==&include_path)
                ));
            }
            drop(recording);
            drop(root);
            drop(budget);
            let live = observer.budget_snapshot();
            assert!(
                live.retained_bytes > baseline,
                "actual provider output/report owners survive their producers"
            );
            if let Some(expected) = &expected {
                assert!(output.warnings.iter().any(|w| w == expected));
                assert!(output.observations.iter().any(|o|matches!(o,ResolutionObservation::Directory {path,complete:true,..} if path==dir.path())));
            }
            drop(output);
            assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
            assert_eq!(observer.budget_snapshot().work, live.work);
            assert_eq!(observer.budget_snapshot().byte_work, live.byte_work);
        }
    }

    #[test]
    fn explicit_structured_owner_actual_original_paths_provenance_outlive_cache() {
        use crate::compile_time::CompileTimeLimits;
        let (dir, _, mut resolver) = physical_report_resolver();
        let path = dir.path().join("Other.pas");
        let entries = vec![ProjectPathEntry {
            path: path.clone(),
            provenance: ProjectPathProvenance::Mapped {
                root: dir.path().into(),
            },
        }];
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let observer = budget
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut budget)
            .unwrap();
        let baseline = budget.snapshot().retained_bytes;
        let call = resolver.store_call(&mut budget).unwrap();
        let group = call
            .explicit_structured_paths(&entries, &NoCancellation)
            .unwrap();
        assert!(
            matches!(group, LookupScratch::Admitted(_)),
            "typed structured Vec/path/provenance must have original independent owner"
        );
        assert_eq!(group[0].0, path);
        assert_eq!(group[0].1, entries[0]);
        let pp = group[0].0.as_os_str().as_encoded_bytes().as_ptr();
        let ep = group[0].1.path.as_os_str().as_encoded_bytes().as_ptr();
        let rp = match &group[0].1.provenance {
            ProjectPathProvenance::Mapped { root } => root.as_os_str().as_encoded_bytes().as_ptr(),
            _ => unreachable!(),
        };
        let spent = budget.snapshot();
        drop(call);
        drop(resolver);
        drop(entries);
        drop(budget);
        assert_eq!(group[0].0.as_os_str().as_encoded_bytes().as_ptr(), pp);
        assert_eq!(group[0].1.path.as_os_str().as_encoded_bytes().as_ptr(), ep);
        assert_eq!(
            match &group[0].1.provenance {
                ProjectPathProvenance::Mapped { root } =>
                    root.as_os_str().as_encoded_bytes().as_ptr(),
                _ => unreachable!(),
            },
            rp
        );
        assert!(observer.budget_snapshot().retained_bytes > baseline);
        drop(group);
        assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
        assert_eq!(observer.budget_snapshot().work, spent.work);
        assert_eq!(observer.budget_snapshot().byte_work, spent.byte_work);
    }

    #[cfg(unix)]
    #[test]
    fn explicit_group_relative_legacy_route_compatibility_probe() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        let (_dir, root, mut resolver) = physical_report_resolver();
        let sibling = tempfile::tempdir().unwrap();
        let unit = sibling.path().join("Selected.pas");
        let importer = sibling.path().join("LegacyMain.pas");
        fs::write(&unit, "unit Selected; interface implementation end.").unwrap();
        fs::write(&importer, "unit LegacyMain; interface implementation end.").unwrap();
        let cwd = std::env::current_dir().unwrap();
        let mut prefix = PathBuf::new();
        for _ in cwd
            .components()
            .filter(|part| matches!(part, Component::Normal(_)))
        {
            prefix.push("..");
        }
        let relative = |path: &Path| prefix.join(path.strip_prefix("/").unwrap());
        let route = LegacyRoute {
            source_path: relative(&importer),
            sibling_directory: relative(sibling.path()),
        };
        assert!(!route.source_path.is_absolute());
        assert!(
            resolver
                .legacy_entry_for_path(&unit, Some(&route))
                .is_some(),
            "existing actual legacy builder authorizes relative route"
        );
        resolver
            .context
            .explicit_units
            .insert("selected".into(), vec![unit.clone()]);
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let outcome = resolver
            .unit_report_capability(
                UnitResolveRequest {
                    requested_name: "Selected",
                    importer_path: &root,
                    legacy_route: Some(&route),
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
        assert!(
            matches!(outcome.result(),Resolution::Found(provider) if provider.unit.source.path==unit)
        );
    }

    #[test]
    fn explicit_structured_owner_real_callback_alias_ambiguity_and_error_escapes() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        for mode in 0..3 {
            let (dir, root, mut resolver) = physical_report_resolver();
            let a = dir.path().join("A.pas");
            let b = dir.path().join("B.pas");
            let rejected = dir.path().join("Rejected.pas");
            fs::write(&a, "unit Other; interface implementation end.").unwrap();
            fs::write(&rejected, "unit Wrong;").unwrap();
            if mode == 2 {
                fs::remove_file(&a).unwrap();
                fs::write(&a, "unit Wrong;").unwrap();
                fs::create_dir(&b).unwrap();
            } else {
                fs::write(&b, "unit Other; interface implementation end.").unwrap();
            }
            let mut paths = vec![rejected.clone(), a.clone()];
            if mode > 0 {
                paths.push(b.clone());
            }
            let entries = paths
                .iter()
                .map(|p| resolver.context.path_entry_for(p).unwrap())
                .collect();
            resolver
                .context
                .unit_aliases
                .insert("alias".into(), "Other".into());
            resolver
                .context
                .explicit_unit_entries
                .insert("other".into(), entries);
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let observer = budget
                .admit_owned(0, 0, 0, || ())
                .unwrap()
                .into_shared(&mut budget)
                .unwrap();
            let baseline = budget.snapshot().retained_bytes;
            let outcome = resolver
                .unit_report_capability(
                    UnitResolveRequest {
                        requested_name: "Alias",
                        importer_path: &root,
                        legacy_route: None,
                    },
                    &mut budget,
                    &NoCancellation,
                )
                .unwrap();
            match mode {
                0 => assert!(
                    matches!(outcome.result(),Resolution::Found(provider) if provider.unit.source.path==a && provider.unit.requested_name=="Alias" && provider.unit.declared_name=="Other")
                ),
                1 => assert!(
                    matches!(outcome.result(),Resolution::Ambiguous {candidates} if candidates.len()==2 && candidates[0].path==a && candidates[1].path==b && candidates.iter().all(|c|c.declared_name.as_deref()==Some("Other")))
                ),
                _ => assert!(
                    matches!(outcome.result(),Resolution::Incomplete {reason,candidates} if !reason.is_empty() && candidates.len()==1 && candidates[0].path==a && candidates[0].declared_name.as_deref()==Some("Wrong"))
                ),
            }
            assert!(
                outcome
                    .reports()
                    .observations()
                    .iter()
                    .any(|o| matches!(o,ResolutionObservation::Payload {path,..} if path==&a))
            );
            if mode == 2 {
                assert!(
                    !outcome.reports().observations().iter().any(
                        |o| matches!(o,ResolutionObservation::Payload {path,..} if path==&rejected)
                    ),
                    "error before later sorted rejected path forbids read"
                );
            } else {
                assert!(outcome.reports().observations().iter().any(
                    |o| matches!(o,ResolutionObservation::Payload {path,..} if path==&rejected)
                ));
            }
            let pointers = match outcome.result() {
                Resolution::Ambiguous { candidates }
                | Resolution::Incomplete { candidates, .. } => candidates
                    .iter()
                    .map(|c| {
                        (
                            c.path.as_os_str().as_encoded_bytes().as_ptr(),
                            c.declared_name.as_ref().unwrap().as_ptr(),
                        )
                    })
                    .collect::<Vec<_>>(),
                Resolution::Found(provider) => vec![(
                    provider
                        .unit
                        .source
                        .path
                        .as_os_str()
                        .as_encoded_bytes()
                        .as_ptr(),
                    provider.unit.declared_name.as_ptr(),
                )],
                _ => unreachable!(),
            };
            let spent = budget.snapshot();
            drop(resolver);
            drop(budget);
            assert!(observer.budget_snapshot().retained_bytes > baseline);
            match outcome.result() {
                Resolution::Ambiguous { candidates }
                | Resolution::Incomplete { candidates, .. } => {
                    for (candidate, (path, name)) in candidates.iter().zip(pointers) {
                        assert_eq!(candidate.path.as_os_str().as_encoded_bytes().as_ptr(), path);
                        assert_eq!(candidate.declared_name.as_ref().unwrap().as_ptr(), name);
                    }
                }
                Resolution::Found(provider) => {
                    assert_eq!(
                        provider
                            .unit
                            .source
                            .path
                            .as_os_str()
                            .as_encoded_bytes()
                            .as_ptr(),
                        pointers[0].0
                    );
                    assert_eq!(provider.unit.declared_name.as_ptr(), pointers[0].1);
                    assert!(provider.unit.source.bytes.starts_with(b"unit Other"));
                }
                _ => unreachable!(),
            }
            assert!(!outcome.reports().observations().is_empty());
            drop(outcome);
            assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
            assert_eq!(observer.budget_snapshot().work, spent.work);
            assert_eq!(observer.budget_snapshot().byte_work, spent.byte_work);
        }
    }

    #[test]
    fn explicit_structured_owner_actual_empty_borrowed_route_foreign_group() {
        use crate::compile_time::CompileTimeLimits;
        let (dir, root, mut resolver) = physical_report_resolver();
        let route = LegacyRoute {
            source_path: root,
            sibling_directory: dir.path().into(),
        };
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let call = resolver.store_call(&mut budget).unwrap();
        let empty = call
            .explicit_structured_paths(&[], &NoCancellation)
            .unwrap();
        assert!(matches!(empty, LookupScratch::Admitted(_)));
        assert_eq!(empty.capacity(), 0);
        drop(empty);
        let entry = call
            .context
            .path_entry_for(&dir.path().join("Other.pas"))
            .unwrap();
        let group = CandidateGroup {
            paths: call
                .explicit_structured_paths(std::slice::from_ref(&entry), &NoCancellation)
                .unwrap(),
            legacy_route: Some(&route),
        };
        assert!(std::ptr::eq(group.legacy_route.unwrap(), &route));
        assert_eq!(group.paths[0].1, entry);
        let (_dir, _, mut foreign_resolver) = physical_report_resolver();
        let mut foreign = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut foreign_call = foreign_resolver.store_call(&mut foreign).unwrap();
        let before = foreign.snapshot();
        assert!(matches!(
            foreign_call.resolve_candidate_group(&group, "Other", "Other", &NoCancellation),
            Err(ResolverError::InvalidRequest(_))
        ));
        assert_eq!(foreign.snapshot(), before);
        assert!(foreign_call.report.observations.is_empty());
    }

    #[test]
    fn explicit_structured_owner_actual_cancel_unwind_and_work_refusal_preserve_prior_source() {
        use crate::compile_time::CompileTimeLimits;
        struct Poll {
            calls: std::sync::atomic::AtomicUsize,
            stop: usize,
            panic: bool,
        }
        impl CancellationToken for Poll {
            fn is_cancelled(&self) -> bool {
                let stop = self.calls.fetch_add(1, Ordering::Relaxed) >= self.stop;
                if stop && self.panic {
                    panic!("structured construction injected unwind")
                }
                stop
            }
        }
        for panic in [false, true] {
            let mut done = false;
            let mut refused = 0;
            for stop in 0..300 {
                let (dir, _, mut resolver) = physical_report_resolver();
                let p = dir.path().join("Other.pas");
                let entries = vec![resolver.context.path_entry_for(&p).unwrap(); 3];
                let mut budget =
                    CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
                let mut call = resolver.store_call(&mut budget).unwrap();
                let prior = call
                    .load_candidate_with_limit(
                        &p,
                        &entries[0],
                        None,
                        SourceKind::Unit,
                        4096,
                        &NoCancellation,
                    )
                    .unwrap();
                let before = budget.snapshot();
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    call.explicit_structured_paths(
                        &entries,
                        &Poll {
                            calls: 0.into(),
                            stop,
                            panic,
                        },
                    )
                }));
                match outcome {
                    Err(_) => {
                        assert!(panic);
                        refused += 1;
                    }
                    Ok(Err(ResolverError::Cancelled)) => {
                        assert!(!panic);
                        refused += 1;
                    }
                    Ok(Ok(group)) => {
                        assert_eq!(group.len(), 3);
                        drop(group);
                        done = true;
                    }
                    Ok(Err(error)) => panic!("unexpected {error:?}"),
                }
                assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
                assert!(budget.snapshot().work >= before.work);
                assert!(budget.snapshot().byte_work >= before.byte_work);
                let cache = call
                    .compile_time_store
                    .as_ref()
                    .unwrap()
                    .view
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                assert_eq!(cache.loaded.len(), 1);
                assert_eq!(
                    cache.loaded.get(&prior.id).unwrap().bytes.as_ptr(),
                    prior.bytes.as_ptr()
                );
                drop(cache);
                assert_eq!(
                    call.report
                        .observations
                        .iter()
                        .filter(|o| matches!(o, ResolutionObservation::Payload { .. }))
                        .count(),
                    1
                );
                let retry = call
                    .explicit_structured_paths(&entries, &NoCancellation)
                    .unwrap();
                drop(retry);
                assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
                if done {
                    break;
                }
            }
            assert!(done);
            assert!(refused > 10);
        }
        fn run(ceiling: Option<usize>) -> usize {
            let (dir, _, mut resolver) = physical_report_resolver();
            let entries = vec![
                resolver
                    .context
                    .path_entry_for(&dir.path().join("Other.pas"))
                    .unwrap();
                3
            ];
            let mut budget = CompileTimeBudget::new(
                CompileTimeLimits {
                    max_work: ceiling.unwrap_or(CompileTimeLimits::default().max_work),
                    ..Default::default()
                },
                &NoCancellation,
            );
            let call = resolver.store_call(&mut budget).unwrap();
            let before = budget.snapshot();
            let out = call.explicit_structured_paths(&entries, &NoCancellation);
            if ceiling.is_some() {
                assert!(matches!(out, Err(ResolverError::LimitExceeded { .. })));
            } else {
                drop(out.unwrap());
            }
            assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
            assert!(call.report.observations.is_empty());
            budget.snapshot().work
        }
        let work = run(None);
        run(Some(work - 1));
    }

    #[test]
    fn explicit_structured_owner_actual_receiving_peak_refusal() {
        use crate::compile_time::CompileTimeLimits;
        struct Trace<'a> {
            owner: &'a AdmittedShared<()>,
            peak: std::cell::Cell<usize>,
        }
        impl CancellationToken for Trace<'_> {
            fn is_cancelled(&self) -> bool {
                self.peak.set(
                    self.peak
                        .get()
                        .max(self.owner.budget_snapshot().retained_bytes),
                );
                false
            }
        }
        fn run(ceiling: Option<usize>) -> usize {
            let (dir, _, mut resolver) = physical_report_resolver();
            let entries = vec![
                resolver
                    .context
                    .path_entry_for(&dir.path().join("Other.pas"))
                    .unwrap();
                4
            ];
            let mut budget = CompileTimeBudget::new(
                CompileTimeLimits {
                    max_retained_bytes: ceiling
                        .unwrap_or(CompileTimeLimits::default().max_retained_bytes),
                    ..Default::default()
                },
                &NoCancellation,
            );
            let observer = budget
                .admit_owned(0, 0, 0, || ())
                .unwrap()
                .into_shared(&mut budget)
                .unwrap();
            let call = resolver.store_call(&mut budget).unwrap();
            let before = budget.snapshot();
            let trace = Trace {
                owner: &observer,
                peak: std::cell::Cell::new(before.retained_bytes),
            };
            let out = call.explicit_structured_paths(&entries, &trace);
            if ceiling.is_some() {
                assert!(matches!(out, Err(ResolverError::LimitExceeded { .. })));
            } else {
                drop(out.unwrap());
            }
            assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
            assert!(budget.snapshot().work > before.work);
            assert!(budget.snapshot().byte_work > before.byte_work);
            assert!(call.report.observations.is_empty());
            let cache = call
                .compile_time_store
                .as_ref()
                .unwrap()
                .view
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            assert!(cache.loaded.is_empty());
            trace.peak.get()
        }
        let peak = run(None);
        run(Some(peak - 1));
    }

    #[test]
    fn explicit_structured_owner_actual_unauthorized_entry_stops_later_payload() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        let (dir, root, mut resolver) = physical_report_resolver();
        let outside = tempfile::tempdir().unwrap();
        let denied = dir.path().join("A.pas");
        let later = dir.path().join("Z.pas");
        fs::write(&denied, "unit Selected;").unwrap();
        fs::write(&later, "unit Selected;").unwrap();
        let unauthorized = ProjectPathEntry {
            path: denied.clone(),
            provenance: ProjectPathProvenance::Mapped {
                root: outside.path().into(),
            },
        };
        let authorized = resolver.context.path_entry_for(&later).unwrap();
        resolver
            .context
            .explicit_unit_entries
            .insert("selected".into(), vec![authorized, unauthorized]);
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let outcome = resolver
            .unit_report_capability(
                UnitResolveRequest {
                    requested_name: "Selected",
                    importer_path: &root,
                    legacy_route: None,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
        assert!(
            matches!(outcome.result(),Resolution::Incomplete {reason,candidates} if reason=="unit candidate could not be loaded: AdmissionUnavailable" && candidates.is_empty()),
            "unmapped provenance must refuse through the actual admitted store bridge: {:?}",
            outcome.result()
        );
        assert!(!outcome.reports().observations().iter().any(|o|matches!(o,ResolutionObservation::Payload {path,..} if path==&denied || path==&later)),"unauthorized first candidate prevents every later payload read");
    }

    #[test]
    fn unit_reason_owner_actual_failure_callback_cancellation_every_poll_preserves_prior_owners() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        struct Poll {
            calls: std::sync::atomic::AtomicUsize,
            stop: usize,
        }
        impl CancellationToken for Poll {
            fn is_cancelled(&self) -> bool {
                self.calls.fetch_add(1, Ordering::Relaxed) >= self.stop
            }
        }
        let mut cancelled = 0;
        let mut completed = false;
        for stop in 0..600 {
            let (dir, root, mut resolver) = physical_report_resolver();
            let wrong = dir.path().join("A.pas");
            let bad = dir.path().join("Z.pas");
            fs::write(&wrong, "unit Wrong;").unwrap();
            fs::create_dir(&bad).unwrap();
            let mut first = resolver.context.path_entry_for(&wrong).unwrap();
            first.path = wrong;
            let mut second = first.clone();
            second.path = bad;
            resolver
                .context
                .explicit_unit_entries
                .insert("broken".into(), vec![first, second]);
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let observer = budget
                .admit_owned(0, 0, 0, || ())
                .unwrap()
                .into_shared(&mut budget)
                .unwrap();
            let baseline = budget.snapshot().retained_bytes;
            let prior = resolver
                .unit_report_capability(
                    UnitResolveRequest {
                        importer_path: &root,
                        requested_name: "Other",
                        legacy_route: None,
                    },
                    &mut budget,
                    &NoCancellation,
                )
                .unwrap();
            let include = resolver
                .include_capability(
                    IncludeResolveRequest {
                        including_path: &root,
                        byte_range: 0..0,
                        requested_name: "absent.inc",
                        legacy_route: None,
                    },
                    &mut budget,
                    &NoCancellation,
                )
                .unwrap();
            match resolver.unit_report_capability(
                UnitResolveRequest {
                    importer_path: &root,
                    requested_name: "Broken",
                    legacy_route: None,
                },
                &mut budget,
                &Poll {
                    calls: 0.into(),
                    stop,
                },
            ) {
                Err(ResolverError::Cancelled) => cancelled += 1,
                Ok(out) => {
                    assert!(out.result.owns_reason());
                    assert!(
                        matches!(out.result(),Resolution::Incomplete {reason,candidates} if reason=="unit candidate is unauthorized" && candidates.len()==1)
                    );
                    completed = true;
                    drop(out);
                }
                Err(other) => panic!("unexpected failure at {stop}: {other:?}"),
            }
            assert!(resolver.callback_unit_reason.is_none());
            assert!(resolver.callback_include_reason.is_none());
            let spent = budget.snapshot();
            drop(resolver);
            drop(budget);
            assert!(
                matches!(prior.result(),Resolution::Found(p) if p.unit.requested_name=="Other")
            );
            assert!(!prior.reports().observations().is_empty());
            assert!(
                matches!(include.result(),Resolution::Unavailable {reason} if reason.contains("absent.inc"))
            );
            drop(prior);
            drop(include);
            assert_eq!(
                observer.budget_snapshot().retained_bytes,
                baseline,
                "every actual failure callback poll refunds only dead owners: {stop}"
            );
            assert_eq!(observer.budget_snapshot().work, spent.work);
            assert_eq!(observer.budget_snapshot().byte_work, spent.byte_work);
            if completed {
                break;
            }
        }
        assert!(completed);
        assert!(cancelled > 20);
    }

    #[test]
    fn unit_reason_owner_original_callback_pointer_transfer_and_foreign_root_refusal() {
        use crate::compile_time::{AdmittedProviderSource, CompileTimeLimits, CompileTimeResolver};
        let (_dir, root, mut resolver) = physical_report_resolver();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let prior = resolver
            .unit_report_capability(
                UnitResolveRequest {
                    importer_path: &root,
                    requested_name: "Other",
                    legacy_route: None,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
        let Resolution::Found(provider) = prior.result() else {
            panic!("physical prior provider")
        };
        let source = provider.unit.source.bytes.as_ptr();
        let requested = provider.unit.requested_name.as_ptr();
        let declared = provider.unit.declared_name.as_ptr();
        let mut call = resolver.callback_report_call();
        call.owning_unit_callback = true;
        call.owning_unit_reason = true;
        let out = CompileTimeResolver::unit_capability(
            &mut *call,
            UnitResolveRequest {
                importer_path: &root,
                requested_name: "Absent",
                legacy_route: None,
            },
            &mut budget,
            &NoCancellation,
        )
        .unwrap();
        let pointer = call.callback_unit_reason.as_ref().unwrap().as_ptr();
        let before = budget.snapshot();
        let out = call.finish(out.result, &mut budget).unwrap();
        assert_eq!(
            budget.snapshot().byte_work,
            before.byte_work,
            "owning callback handoff cannot copy the original reason payload"
        );
        let (result, reports) = out.into_parts();
        assert!(
            matches!(result.result(),Resolution::Unavailable {reason} if reason.as_ptr()==pointer)
        );
        let mut receiver = budget
            .admit_owned(0, 0, 0, || None::<Resolution<AdmittedProviderSource>>)
            .unwrap();
        let before = budget.snapshot();
        result
            .move_reason_into(&mut receiver, &budget, |to, value| *to = Some(value))
            .unwrap();
        assert_eq!(budget.snapshot().byte_work, before.byte_work);
        assert!(
            matches!(&*receiver,Some(Resolution::Unavailable {reason}) if reason.as_ptr()==pointer)
        );
        let other_out = resolver
            .unit_report_capability(
                UnitResolveRequest {
                    importer_path: &root,
                    requested_name: "Absent",
                    legacy_route: None,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
        let (other_result, other_reports) = other_out.into_parts();
        let mut other = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut foreign = other
            .admit_owned(0, 0, 0, || None::<Resolution<AdmittedProviderSource>>)
            .unwrap();
        let before = other.snapshot();
        assert!(
            other_result
                .move_reason_into(&mut foreign, &other, |to, value| *to = Some(value))
                .is_err()
        );
        assert_eq!(other.snapshot(), before);
        assert!(foreign.is_none());
        assert_eq!(provider.unit.source.bytes.as_ptr(), source);
        assert_eq!(provider.unit.requested_name.as_ptr(), requested);
        assert_eq!(provider.unit.declared_name.as_ptr(), declared);
        assert!(!prior.reports().observations().is_empty());
        assert!(reports.warnings().is_empty());
        assert!(other_reports.warnings().is_empty());
    }

    #[test]
    fn unit_reason_owner_actual_partial_candidate_error_and_incomplete_tiers() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        for scenario in 0..6 {
            let (dir, root, mut resolver) = physical_report_resolver();
            if scenario == 0 {
                let wrong = dir.path().join("A.pas");
                let bad = dir.path().join("Z.pas");
                fs::write(&wrong, "unit Wrong;").unwrap();
                fs::create_dir(&bad).unwrap();
                let mut first = resolver.context.path_entry_for(&wrong).unwrap();
                first.path = wrong;
                let mut second = first.clone();
                second.path = bad;
                resolver
                    .context
                    .explicit_unit_entries
                    .insert("broken".into(), vec![first, second]);
            } else if scenario == 1 {
                resolver.context.discovery_complete = false;
                resolver.context.binding_metadata_complete = false;
            } else if scenario == 2 {
                resolver.context.packages = vec!["UnadmittedPackage".into()];
            } else if scenario == 3 {
                resolver.limits.max_source_bytes = 1;
            } else if scenario == 4 {
                let missing = dir.path().join("Never.pas");
                let entry = resolver.context.path_entry_for(&missing).unwrap();
                resolver
                    .context
                    .path_issues
                    .push(pascal_project::ProjectPathIssue {
                        kind: pascal_project::ProjectPathIssueKind::MissingReference,
                        source_file: root.clone(),
                        property: "DCCReference".into(),
                        raw: "Never.pas".into(),
                        path: Some(missing),
                        unit_name: Some("Absent".into()),
                        provenance: entry.provenance,
                    });
            } else {
                resolver.limits.max_directory_entries = 0;
            }
            let requested = if scenario == 0 {
                "Broken"
            } else if scenario == 3 {
                "Other"
            } else {
                "Absent"
            };
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let out = resolver
                .unit_report_capability(
                    UnitResolveRequest {
                        importer_path: &root,
                        requested_name: requested,
                        legacy_route: None,
                    },
                    &mut budget,
                    &NoCancellation,
                )
                .unwrap();
            let Resolution::Incomplete { reason, candidates } = out.result() else {
                panic!("actual incomplete unit scenario {scenario}")
            };
            assert!(!reason.is_empty());
            assert!(out.result.owns_reason());
            if scenario == 0 {
                assert_eq!(reason, "unit candidate is unauthorized");
                assert!(
                    out.reports().warnings().iter().any(
                        |warning| warning.contains("Z.pas") && warning.contains("unauthorized")
                    )
                );
                assert_eq!(candidates.len(), 1);
                assert!(candidates[0].path.ends_with("A.pas"));
                assert_eq!(candidates[0].declared_name.as_deref(), Some("Wrong"));
                assert!(out.reports().observations().iter().any(|o|matches!(o,ResolutionObservation::Payload {path,..} if path.ends_with("A.pas"))));
            }
            if scenario == 1 {
                assert_eq!(reason, "project metadata is incomplete");
            }
            if scenario == 2 {
                assert_eq!(
                    reason,
                    "package metadata has no admitted compile-time reader"
                );
            }
            if scenario == 3 {
                assert!(reason.contains("TooLarge"));
            }
            if scenario == 4 {
                assert!(reason.starts_with("explicit unit Absent is missing:"));
                assert!(reason.ends_with("Never.pas"));
            }
            if scenario == 5 {
                assert!(reason.starts_with("directory scan under "));
                assert!(reason.ends_with("was incomplete"));
                assert!(out.reports().observations().iter().any(|o| matches!(
                    o,
                    ResolutionObservation::Directory {
                        complete: false,
                        ..
                    }
                )));
            }
            let pointer = reason.as_ptr();
            let (result, reports) = out.into_parts();
            drop(reports);
            drop(resolver);
            drop(budget);
            assert!(
                matches!(result.result(),Resolution::Incomplete {reason,..} if reason.as_ptr()==pointer)
            );
            assert!(
                result.into_unmigrated().is_err(),
                "candidate-bearing result cannot naked-extract its migrated reason"
            );
        }
    }

    #[test]
    fn unit_reason_owner_actual_constructor_refuses_before_format_and_unwinds() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        struct Format<'a>(&'a std::cell::Cell<usize>, bool);
        impl std::fmt::Display for Format<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.set(self.0.get() + 1);
                if self.1 {
                    panic!("unit reason formatter unwind")
                }
                f.write_str("unit failure")
            }
        }
        let (_dir, root, mut resolver) = physical_report_resolver();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let prior = resolver
            .unit_report_capability(
                UnitResolveRequest {
                    importer_path: &root,
                    requested_name: "Other",
                    legacy_route: None,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
        let calls = std::cell::Cell::new(0);
        let before = budget.snapshot();
        budget.constrain_retained(before.retained_bytes).unwrap();
        let mut call = resolver.callback_report_call();
        call.owning_unit_reason = true;
        {
            let mut store = call.store_call(&mut budget).unwrap();
            let b = store.store_budget(&NoCancellation).unwrap();
            let before = b.snapshot();
            assert!(matches!(
                store.unit_reason_build(
                    128,
                    &NoCancellation,
                    format_args!("{}", Format(&calls, false))
                ),
                Err(ResolverError::LimitExceeded { .. })
            ));
            assert_eq!(calls.get(), 0);
            assert_eq!(b.snapshot(), before);
        }
        drop(call);
        let mut fresh = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let observer = fresh
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut fresh)
            .unwrap();
        let baseline = fresh.snapshot().retained_bytes;
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut call = resolver.callback_report_call();
            call.owning_unit_reason = true;
            let mut store = call.store_call(&mut fresh).unwrap();
            let _ = store.unit_reason_build(
                128,
                &NoCancellation,
                format_args!("{}", Format(&calls, true)),
            );
        }));
        assert!(unwind.is_err());
        assert_eq!(calls.get(), 1);
        assert!(resolver.callback_unit_reason.is_none());
        assert!(!resolver.owning_unit_reason);
        let spent = fresh.snapshot();
        drop(resolver);
        assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
        assert!(spent.work > 0);
        assert!(spent.byte_work >= 128);
        assert!(matches!(prior.result(), Resolution::Found(_)));
        assert!(!prior.reports().observations().is_empty());
    }

    #[test]
    fn found_unit_name_owner_foreign_constructor_and_promotion_preserve_receiving_root() {
        use crate::compile_time::{CompileTimeLimits, ProviderSource};
        let (_dir, root, mut resolver) = physical_report_resolver();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut callback = resolver.callback_report_call();
        callback.owning_unit_callback = true;
        let out = callback
            .store_call(&mut budget)
            .unwrap()
            .try_resolve_unit(
                UnitResolveRequest {
                    importer_path: &root,
                    requested_name: "Other",
                    legacy_route: None,
                },
                &NoCancellation,
            )
            .unwrap();
        let Resolution::Found(unit) = out.result else {
            panic!("physical unit")
        };
        let names = callback.callback_unit_names.take().unwrap();
        let backing = ProviderSource::admit_cached_backing(&unit.source, &mut budget).unwrap();
        let pointer = backing.bytes.as_ptr();
        let Some(CallbackReports::Built(reports)) = callback.callback_reports.take() else {
            panic!("actual reports")
        };
        let report_pointer = reports.observations().as_ptr();
        let mut other = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let before = other.snapshot();
        assert!(
            callback
                .build_owned_provider(unit, names, "Other", &mut other, &NoCancellation)
                .is_err()
        );
        assert_eq!(other.snapshot(), before);
        assert_eq!(backing.bytes.as_ptr(), pointer);
        assert_eq!(reports.observations().as_ptr(), report_pointer);
        let out = callback
            .store_call(&mut budget)
            .unwrap()
            .try_resolve_unit(
                UnitResolveRequest {
                    importer_path: &root,
                    requested_name: "Other",
                    legacy_route: None,
                },
                &NoCancellation,
            )
            .unwrap();
        let Resolution::Found(unit) = out.result else {
            panic!("physical unit")
        };
        let names = callback.callback_unit_names.take().unwrap();
        let provider = callback
            .build_owned_provider(unit, names, "Other", &mut budget, &NoCancellation)
            .unwrap();
        assert!(ProviderSource::promote_owned(provider, backing.clone(), &mut other).is_err());
        assert_eq!(other.snapshot(), before);
        assert!(!backing.bytes.is_empty());
        assert!(!reports.observations().is_empty());
    }

    #[test]
    fn found_unit_name_owner_alias_namespace_last_provider_refund_keeps_source_and_reports() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        for requested in ["Alias", "Named"] {
            let (dir, root, mut resolver) = physical_report_resolver();
            fs::write(
                dir.path().join("Space.Other.pas"),
                "{comment} unit Space . Other; interface type T=Int64; implementation end.",
            )
            .unwrap();
            fs::write(
                dir.path().join("Space.Named.pas"),
                "unit Space.Named; interface type T=Int64; implementation end.",
            )
            .unwrap();
            resolver
                .context
                .unit_aliases
                .insert("Alias".into(), "Space.Other".into());
            resolver.context.unit_namespaces = vec!["Space".into()];
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let observer = budget
                .admit_owned(0, 0, 0, || ())
                .unwrap()
                .into_shared(&mut budget)
                .unwrap();
            let baseline = budget.snapshot().retained_bytes;
            let out = resolver
                .unit_report_capability(
                    UnitResolveRequest {
                        importer_path: &root,
                        requested_name: requested,
                        legacy_route: None,
                    },
                    &mut budget,
                    &NoCancellation,
                )
                .unwrap();
            let (result, reports) = out.into_parts();
            let Resolution::Found(provider) = result.into_unmigrated().unwrap() else {
                panic!("physical named provider")
            };
            assert_eq!(provider.unit.requested_name, requested);
            assert_eq!(
                provider.unit.declared_name,
                if requested == "Alias" {
                    "Space.Other"
                } else {
                    "Space.Named"
                }
            );
            assert!(
                provider
                    .authorized_qualifiers
                    .iter()
                    .any(|n| n == requested)
            );
            let requested_pointer = provider.unit.requested_name.as_ptr();
            let declared_pointer = provider.unit.declared_name.as_ptr();
            let backing = provider
                .unit
                .source
                .compile_time_backing
                .as_ref()
                .unwrap()
                .clone();
            let source_pointer = backing.bytes.as_ptr();
            let report_pointer = reports.observations().as_ptr();
            let last = provider.clone();
            drop(resolver);
            drop(budget);
            let live = observer.budget_snapshot();
            drop(provider);
            assert_eq!(
                observer.budget_snapshot(),
                live,
                "non-last provider Arc drop cannot refund live original names"
            );
            assert_eq!(last.unit.requested_name.as_ptr(), requested_pointer);
            assert_eq!(last.unit.declared_name.as_ptr(), declared_pointer);
            drop(last);
            assert!(observer.budget_snapshot().retained_bytes < live.retained_bytes);
            assert_eq!(backing.bytes.as_ptr(), source_pointer);
            assert_eq!(reports.observations().as_ptr(), report_pointer);
            assert!(!reports.observations().is_empty());
            assert!(observer.budget_snapshot().retained_bytes > baseline);
            drop(backing);
            drop(reports);
            assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
            assert_eq!(observer.budget_snapshot().work, live.work);
            assert_eq!(observer.budget_snapshot().byte_work, live.byte_work);
        }
    }

    #[test]
    fn found_unit_name_owner_physical_headers_refund_individually_and_refuse_before_parse_copy() {
        use crate::compile_time::CompileTimeLimits;
        let (dir, _, mut resolver) = physical_report_resolver();
        let texts = [
            "unit Wrong;",
            "{comment}\nunit Space . Other;",
            "{$IFDEF NO} unit Other;",
            "// line\n(* comment *) unit _Name123;",
            "unit Bad..Name;",
            "unit Other; implementation end.",
        ];
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        for (index, text) in texts.iter().enumerate() {
            let path = dir.path().join(format!("Header{index}.pas"));
            fs::write(&path, text).unwrap();
            let entry = resolver.context.path_entry_for(&path).unwrap();
            let source = resolver
                .store
                .load_admitted(
                    SourceRequest {
                        path: &path,
                        entry: &entry,
                        read_policy: &resolver.context.read_policy,
                        legacy_route: None,
                        kind: SourceKind::Unit,
                        max_bytes: 4096,
                    },
                    &mut budget,
                    &NoCancellation,
                )
                .unwrap();
            let before = budget.snapshot();
            let name = owned_header_name(
                source.analysis_bytes().as_ref(),
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
            assert_eq!(
                name.as_ref().map(|s| s.as_str()),
                admitted_header_name(source.analysis_bytes().as_ref())
                    .map(canonical_unit_name)
                    .as_deref(),
                "bounded header semantics unchanged for {text}"
            );
            if let Some(name) = name {
                assert_eq!(
                    budget.snapshot().retained_bytes,
                    before.retained_bytes
                        + std::mem::size_of::<AdmittedOwned<String>>()
                        + name.capacity()
                );
                let spent = budget.snapshot();
                drop(name);
                assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
                assert_eq!(budget.snapshot().work, spent.work);
                assert_eq!(budget.snapshot().byte_work, spent.byte_work);
            } else {
                assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
            }
            assert!(!source.bytes.is_empty());
        }
        let before = budget.snapshot();
        budget.constrain_retained(before.retained_bytes).unwrap();
        assert!(matches!(
            owned_header_name(b"unit Other;", &mut budget, &NoCancellation),
            Err(ResolverError::LimitExceeded { .. })
        ));
        assert_eq!(budget.snapshot(), before);
        let mut limits = CompileTimeLimits::default();
        limits.max_work = 0;
        let mut exhausted = CompileTimeBudget::new(limits, &NoCancellation);
        assert!(matches!(
            owned_header_name(b"unit Other;", &mut exhausted, &NoCancellation),
            Err(ResolverError::LimitExceeded { .. })
        ));
        assert_eq!(exhausted.snapshot().work, 0);
        // Parser control fits, copied name/control does not. The admitted parse
        // walk remains spent on this later receiving refusal.
        let limits = CompileTimeLimits::default();
        let mut tight = CompileTimeBudget::new(limits, &NoCancellation);
        let observer = tight
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut tight)
            .unwrap();
        let base = tight.snapshot();
        tight
            .constrain_retained(base.retained_bytes + std::mem::size_of::<AdmittedOwned<()>>())
            .unwrap();
        assert!(matches!(
            owned_header_name(b"unit Other;", &mut tight, &NoCancellation),
            Err(ResolverError::LimitExceeded { .. })
        ));
        assert_eq!(tight.snapshot().retained_bytes, base.retained_bytes);
        assert_eq!(tight.snapshot().work, base.work + 8 * b"unit Other;".len());
        assert_eq!(observer.budget_snapshot().work, tight.snapshot().work);
    }

    #[test]
    fn found_unit_name_owner_actual_callback_cancel_error_and_header_unwind_drop_temporary_names() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        struct Poll {
            calls: std::sync::atomic::AtomicUsize,
            stop: usize,
            panic: bool,
        }
        impl CancellationToken for Poll {
            fn is_cancelled(&self) -> bool {
                let stopped = self.calls.fetch_add(1, Ordering::Relaxed) >= self.stop;
                if stopped && self.panic {
                    panic!("owned header cancellation callback unwind")
                }
                stopped
            }
        }
        let mut complete = false;
        let mut cancelled = 0;
        for stop in 0..500 {
            let (dir, root, mut resolver) = physical_report_resolver();
            fs::write(
                dir.path().join("Other.pas"),
                "unit Rejected; interface end.",
            )
            .unwrap();
            fs::write(
                dir.path().join("Space.Other.pas"),
                "unit Space.Other; interface implementation end.",
            )
            .unwrap();
            resolver.context.unit_namespaces = vec!["Space".into()];
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let observer = budget
                .admit_owned(0, 0, 0, || ())
                .unwrap()
                .into_shared(&mut budget)
                .unwrap();
            let baseline = budget.snapshot().retained_bytes;
            let prior = resolver
                .include_capability(
                    IncludeResolveRequest {
                        including_path: &root,
                        byte_range: 0..0,
                        requested_name: "value.inc",
                        legacy_route: None,
                    },
                    &mut budget,
                    &NoCancellation,
                )
                .unwrap();
            match resolver.unit_report_capability(
                UnitResolveRequest {
                    importer_path: &root,
                    requested_name: "Other",
                    legacy_route: None,
                },
                &mut budget,
                &Poll {
                    calls: 0.into(),
                    stop,
                    panic: false,
                },
            ) {
                Err(ResolverError::Cancelled) => cancelled += 1,
                Ok(out) => {
                    assert!(
                        matches!(out.result(),Resolution::Found(p) if p.unit.requested_name=="Other" && p.unit.declared_name=="Space.Other")
                    );
                    drop(out);
                    complete = true;
                }
                Err(other) => panic!("unexpected unit error at {stop}: {other:?}"),
            }
            let spent = budget.snapshot();
            drop(resolver);
            drop(budget);
            assert!(matches!(prior.result(), Resolution::Found(_)));
            drop(prior);
            assert_eq!(
                observer.budget_snapshot().retained_bytes,
                baseline,
                "no name/coordinator/provider leak at poll {stop}"
            );
            assert_eq!(observer.budget_snapshot().work, spent.work);
            assert_eq!(observer.budget_snapshot().byte_work, spent.byte_work);
            if complete {
                break;
            }
        }
        assert!(complete);
        assert!(cancelled > 20);
        let (dir, root, mut resolver) = physical_report_resolver();
        fs::remove_file(dir.path().join("Other.pas")).unwrap();
        fs::create_dir(dir.path().join("Other.pas")).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let observer = budget
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut budget)
            .unwrap();
        let baseline = budget.snapshot().retained_bytes;
        let out = resolver
            .unit_report_capability(
                UnitResolveRequest {
                    importer_path: &root,
                    requested_name: "Other",
                    legacy_route: None,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
        assert!(matches!(
            out.result(),
            Resolution::Incomplete { .. } | Resolution::Unavailable { .. }
        ));
        drop(out);
        drop(resolver);
        assert_eq!(budget.snapshot().retained_bytes, baseline);
        let before = budget.snapshot();
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = owned_header_name(
                b"{long comment} unit Space.Other;",
                &mut budget,
                &Poll {
                    calls: 0.into(),
                    stop: 12,
                    panic: true,
                },
            );
        }));
        assert!(unwind.is_err());
        assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
        assert!(budget.snapshot().work > before.work);
        assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
    }

    #[test]
    fn include_reason_owner_transfer_no_copy_work_and_foreign_root_refusal() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        let (_dir, root, mut resolver) = physical_report_resolver();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let out = resolver
            .include_capability(
                IncludeResolveRequest {
                    including_path: &root,
                    byte_range: 0..0,
                    requested_name: "absent.inc",
                    legacy_route: None,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
        let (result, reports) = out.into_parts();
        let pointer = match result.result() {
            Resolution::Unavailable { reason } => reason.as_ptr(),
            _ => panic!("reason"),
        };
        let mut destination = budget.admit_owned(0, 0, 0, || None).unwrap();
        let before = budget.snapshot();
        result
            .move_reason_into(&mut destination, &budget, |dest, value| *dest = Some(value))
            .unwrap();
        assert!(
            matches!(&*destination,Some(Resolution::Unavailable {reason}) if reason.as_ptr()==pointer)
        );
        assert_eq!(budget.snapshot().work, before.work);
        assert_eq!(budget.snapshot().byte_work, before.byte_work);
        let out = resolver
            .include_capability(
                IncludeResolveRequest {
                    including_path: &root,
                    byte_range: 0..0,
                    requested_name: "",
                    legacy_route: None,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
        let (result, other_reports) = out.into_parts();
        let mut other = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut wrong = other.admit_owned(0, 0, 0, || None).unwrap();
        let before = other.snapshot();
        assert!(
            result
                .move_reason_into(&mut wrong, &other, |dest, value| *dest = Some(value))
                .is_err()
        );
        assert!(wrong.is_none());
        assert_eq!(other.snapshot(), before);
        assert!(!reports.observations().is_empty());
        drop(other_reports);
        assert!(destination.is_some());
    }

    #[test]
    fn include_reason_owner_preformat_refusal_cancel_and_formatter_unwind_preserve_live_fields() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        let (_dir, root, mut resolver) = physical_report_resolver();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let source = resolver
            .include_capability(
                IncludeResolveRequest {
                    including_path: &root,
                    byte_range: 0..0,
                    requested_name: "value.inc",
                    legacy_route: None,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
        let before = budget.snapshot();
        budget.constrain_retained(before.retained_bytes).unwrap();
        let calls = std::cell::Cell::new(0);
        struct Format<'a>(&'a std::cell::Cell<usize>, bool);
        impl fmt::Display for Format<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.set(self.0.get() + 1);
                if self.1 {
                    panic!("reason formatter unwind")
                }
                f.write_str("reason")
            }
        }
        {
            let mut callback = resolver.callback_report_call();
            callback.owning_include_callback = true;
            let mut call = callback.store_call(&mut budget).unwrap();
            assert!(matches!(
                call.include_reason_build(
                    128,
                    &NoCancellation,
                    format_args!("{}", Format(&calls, false))
                ),
                Err(ResolverError::LimitExceeded { .. })
            ));
            assert!(call.callback_include_reason.is_none());
        }
        assert_eq!(calls.get(), 0);
        assert_eq!(budget.snapshot(), before);
        // A new root permits the real constructor peak; errors/cancellation and
        // unwind must not leave its independently minted String credit behind.
        let mut fresh = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let observer = fresh
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut fresh)
            .unwrap();
        let baseline = fresh.snapshot().retained_bytes;
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut callback = resolver.callback_report_call();
            callback.owning_include_callback = true;
            let mut call = callback.store_call(&mut fresh).unwrap();
            let _ = call.include_reason_build(
                128,
                &NoCancellation,
                format_args!("{}", Format(&calls, true)),
            );
        }));
        assert!(unwind.is_err());
        assert!(resolver.callback_include_reason.is_none());
        struct Cancel;
        impl CancellationToken for Cancel {
            fn is_cancelled(&self) -> bool {
                true
            }
        }
        {
            let mut callback = resolver.callback_report_call();
            callback.owning_include_callback = true;
            let mut call = callback.store_call(&mut fresh).unwrap();
            assert!(matches!(
                call.include_reason_build(128, &Cancel, format_args!("{}", Format(&calls, false))),
                Err(ResolverError::Cancelled)
            ));
        }
        assert_eq!(calls.get(), 1);
        assert!(matches!(source.result(), Resolution::Found(_)));
        assert!(!source.reports().observations().is_empty());
        drop(resolver);
        let spent = fresh.snapshot();
        assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
        assert!(spent.work > 0);
        assert!(spent.byte_work > 0);
    }

    #[test]
    fn case_match_f1_exhausted_work_refuses_before_preflight_body() {
        use crate::compile_time::CompileTimeLimits;
        struct Probe(std::sync::atomic::AtomicUsize);
        impl CancellationToken for Probe {
            fn is_cancelled(&self) -> bool {
                self.0.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
        let (dir, _, mut resolver) = directory_probe_resolver();
        let entry = resolver.context.path_entry_for(dir.path()).unwrap();
        let mut root = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut call = resolver.store_call(&mut root).unwrap();
        let listing = call
            .list_directory(dir.path(), &entry, None, &NoCancellation, false)
            .unwrap();
        let pointer = listing.files.as_ptr();
        let mut limits = CompileTimeLimits::default();
        limits.max_work = 0;
        let mut budget = CompileTimeBudget::new(limits, &NoCancellation);
        let probe = Probe(0.into());
        assert!(matches!(
            case_sort_sizing(
                &listing,
                std::ffi::OsStr::new("value.inc"),
                &mut budget,
                &probe
            ),
            Err(ResolverError::LimitExceeded { .. })
        ));
        assert!(matches!(
            relative_components_owned(
                dir.path(),
                &dir.path().join("value.inc"),
                &mut budget,
                &probe
            ),
            Err(ResolverError::LimitExceeded { .. })
        ));
        // An invalid index is a sentinel: preflight work refusal must precede
        // any receiving-payload index access, not merely copying the vector.
        assert!(matches!(
            copy_case_paths(&listing, &[usize::MAX], &mut budget, &probe),
            Err(ResolverError::LimitExceeded { .. })
        ));
        assert!(matches!(
            case_result_payload_size(&listing.files, Some(&mut budget), &probe),
            Err(ResolverError::LimitExceeded { .. })
        ));
        assert_eq!(
            probe.0.load(Ordering::Relaxed),
            0,
            "no scan-body cancellation poll/index/path access before work admission"
        );
        assert_eq!(budget.snapshot().work, 0);
        assert_eq!(budget.snapshot().retained_bytes, 0);
        assert_eq!(listing.files.as_ptr(), pointer);
    }

    #[test]
    fn include_reason_owner_actual_error_callback_cancel_every_poll_refunds_only_dead_storage() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        struct Poll {
            calls: std::sync::atomic::AtomicUsize,
            stop: usize,
        }
        impl CancellationToken for Poll {
            fn is_cancelled(&self) -> bool {
                self.calls.fetch_add(1, Ordering::Relaxed) >= self.stop
            }
        }
        let mut cancelled = 0;
        let mut complete = false;
        for stop in 0..300 {
            let (dir, root, mut resolver) = physical_report_resolver();
            fs::create_dir(dir.path().join("bad.inc")).unwrap();
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let observer = budget
                .admit_owned(0, 0, 0, || ())
                .unwrap()
                .into_shared(&mut budget)
                .unwrap();
            let baseline = budget.snapshot().retained_bytes;
            let source = resolver
                .include_capability(
                    IncludeResolveRequest {
                        including_path: &root,
                        byte_range: 0..0,
                        requested_name: "value.inc",
                        legacy_route: None,
                    },
                    &mut budget,
                    &NoCancellation,
                )
                .unwrap();
            match resolver.include_capability(
                IncludeResolveRequest {
                    including_path: &root,
                    byte_range: 0..0,
                    requested_name: "bad.inc",
                    legacy_route: None,
                },
                &mut budget,
                &Poll {
                    calls: 0.into(),
                    stop,
                },
            ) {
                Err(ResolverError::Cancelled) => cancelled += 1,
                Ok(out) => {
                    assert!(
                        matches!(out.result(),Resolution::Incomplete {reason,..} if reason.contains("bad.inc"))
                    );
                    assert!(out.result.owns_reason());
                    drop(out);
                    complete = true;
                }
                Err(other) => {
                    panic!("unexpected include callback failure at poll {stop}: {other:?}")
                }
            }
            let spent = budget.snapshot();
            drop(resolver);
            drop(budget);
            assert!(
                matches!(source.result(),Resolution::Found(s) if !s.analysis_bytes().is_empty())
            );
            assert!(!source.reports().observations().is_empty());
            assert!(
                observer.budget_snapshot().retained_bytes > baseline,
                "live previous source/reports retain their own credits"
            );
            drop(source);
            assert_eq!(
                observer.budget_snapshot().retained_bytes,
                baseline,
                "no leaked partial callback/reason owner at poll {stop}"
            );
            assert_eq!(observer.budget_snapshot().work, spent.work);
            assert_eq!(observer.budget_snapshot().byte_work, spent.byte_work);
            if complete {
                break;
            }
        }
        assert!(complete);
        assert!(cancelled > 10);
    }

    #[test]
    fn case_match_f1_long_preflights_poll_and_spend_work_without_copying() {
        use crate::compile_time::CompileTimeLimits;
        struct Poll {
            calls: std::sync::atomic::AtomicUsize,
            stop: usize,
        }
        impl CancellationToken for Poll {
            fn is_cancelled(&self) -> bool {
                self.calls.fetch_add(1, Ordering::Relaxed) >= self.stop
            }
        }
        let (dir, _, mut resolver) = directory_probe_resolver();
        for index in 0..20 {
            fs::write(dir.path().join(format!("extra{index}.inc")), "{extra}").unwrap();
        }
        let entry = resolver.context.path_entry_for(dir.path()).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut call = resolver.store_call(&mut budget).unwrap();
        let listing = call
            .list_directory(dir.path(), &entry, None, &NoCancellation, false)
            .unwrap();
        let pointer = listing.files.as_ptr();
        let before = budget.snapshot();
        let poll = || Poll {
            calls: 0.into(),
            stop: 5,
        };
        assert!(matches!(
            case_sort_sizing(
                &listing,
                std::ffi::OsStr::new("value.inc"),
                &mut budget,
                &poll()
            ),
            Err(ResolverError::Cancelled)
        ));
        assert_eq!(
            budget.snapshot().work,
            before.work + listing.files.len() + listing.directories.len()
        );
        let work = budget.snapshot().work;
        let path = dir.path().join("a/b/c/d/e/f/g/h/i/j");
        assert!(matches!(
            relative_components_owned(dir.path(), &path, &mut budget, &poll()),
            Err(ResolverError::Cancelled)
        ));
        assert_eq!(
            budget.snapshot().work,
            work + dir.path().as_os_str().len() + path.as_os_str().len() + 2
        );
        let work = budget.snapshot().work;
        let matches = (0..listing.files.len()).collect::<Vec<_>>();
        assert!(matches!(
            copy_case_paths(&listing, &matches, &mut budget, &poll()),
            Err(ResolverError::Cancelled)
        ));
        assert_eq!(budget.snapshot().work, work + matches.len());
        let work = budget.snapshot().work;
        assert!(matches!(
            case_result_payload_size(&listing.files, Some(&mut budget), &poll()),
            Err(ResolverError::Cancelled)
        ));
        assert_eq!(budget.snapshot().work, work + listing.files.len());
        assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
        assert_eq!(budget.snapshot().byte_work, before.byte_work);
        assert_eq!(listing.files.as_ptr(), pointer);
        let before = budget.snapshot();
        budget.constrain_retained(before.retained_bytes).unwrap();
        let bytes = case_sort_sizing(
            &listing,
            std::ffi::OsStr::new("value.inc"),
            &mut budget,
            &NoCancellation,
        )
        .unwrap();
        assert!(budget.admit_owned(1, bytes, bytes, || ()).is_err());
        assert_eq!(
            budget.snapshot().work,
            before.work + listing.files.len() + listing.directories.len(),
            "sort construction refusal cannot unspend sizing"
        );
        let before = budget.snapshot();
        let bytes =
            case_result_payload_size(&listing.files, Some(&mut budget), &NoCancellation).unwrap();
        assert!(
            budget
                .admit_owned(listing.files.len(), bytes, bytes, || ())
                .is_err()
        );
        assert_eq!(
            budget.snapshot().work,
            before.work + listing.files.len(),
            "plain receiving construction refuses only after its sizing charge"
        );
        assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
    }

    #[cfg(unix)]
    #[test]
    fn case_match_owner_missing_fallback_does_not_authorize_denied_or_dangling_payloads() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        let (dir, root, mut resolver) = physical_report_resolver();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.inc");
        fs::write(&secret, "{not authorized}").unwrap();
        std::os::unix::fs::symlink(&secret, dir.path().join("Denied.inc")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("gone.inc"),
            dir.path().join("Dangling.inc"),
        )
        .unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        for name in ["Denied.inc", "denied.INC", "Dangling.inc"] {
            let out = resolver
                .include_capability(
                    IncludeResolveRequest {
                        including_path: &root,
                        byte_range: 0..0,
                        requested_name: name,
                        legacy_route: None,
                    },
                    &mut budget,
                    &NoCancellation,
                )
                .unwrap();
            assert!(
                !matches!(out.result(), Resolution::Found(_)),
                "no unauthorized matched payload for {name}"
            );
            assert!(!out.reports().observations().iter().any(|o|matches!(o,ResolutionObservation::Payload {path,..} if path==&secret || path.ends_with("Denied.inc") || path.ends_with("Dangling.inc"))));
        }
    }

    #[test]
    fn case_match_owner_budget_sweep_reaches_matching_and_refunds_all_failed_peaks() {
        use crate::compile_time::CompileTimeLimits;
        let mut refused_before_read = false;
        let mut refused_after_read = false;
        let mut complete = false;
        for headroom in (0..=65536).step_by(256) {
            let (dir, _, mut resolver) = directory_probe_resolver();
            for index in 0..8 {
                fs::write(dir.path().join(format!("Extra{index}.pas")), "unit Extra;").unwrap();
            }
            let entry = resolver.context.path_entry_for(dir.path()).unwrap();
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let observer = budget
                .admit_owned(0, 0, 0, || ())
                .unwrap()
                .into_shared(&mut budget)
                .unwrap();
            let baseline = budget.snapshot().retained_bytes;
            {
                let mut call = resolver.store_call(&mut budget).unwrap();
                drop(
                    call.list_directory(dir.path(), &entry, None, &NoCancellation, false)
                        .unwrap(),
                );
            }
            budget
                .constrain_retained(budget.snapshot().retained_bytes + headroom)
                .unwrap();
            let mut call = resolver.store_call(&mut budget).unwrap();
            let calls = call.store.calls;
            match call.resolve_case_insensitive_path(
                dir.path(),
                &dir.path().join("VaLuE.InC"),
                &entry,
                None,
                &NoCancellation,
            ) {
                Err(ResolverError::LimitExceeded { .. }) => {
                    if call.store.calls == calls {
                        refused_before_read = true;
                    } else {
                        refused_after_read = true;
                    }
                }
                Ok(CaseInsensitiveLookup::Found(path)) => {
                    assert!(path.ends_with("value.inc"));
                    drop(path);
                    complete = true;
                }
                Err(other) => panic!("unexpected budget refusal at {headroom}: {other:?}"),
                _ => panic!("unexpected budget match at {headroom}"),
            }
            let spent = budget.snapshot();
            drop(call);
            drop(resolver);
            assert_eq!(
                observer.budget_snapshot().retained_bytes,
                baseline,
                "all construction/storage retired at headroom {headroom}"
            );
            assert_eq!(budget.snapshot().work, spent.work);
            assert_eq!(budget.snapshot().byte_work, spent.byte_work);
            if complete {
                break;
            }
        }
        assert!(
            refused_before_read && refused_after_read && complete,
            "exercise early and late actual helper peaks through first success"
        );
    }

    #[test]
    fn directory_ownership_filesystem_caller_shares_cache_backing() {
        use crate::compile_time::CompileTimeLimits;
        let (dir, _root, mut resolver) = physical_report_resolver();
        let entry = resolver.context.path_entry_for(dir.path()).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut call = resolver.store_call(&mut budget).unwrap();
        let listing = call
            .list_directory(dir.path(), &entry, None, &NoCancellation, false)
            .unwrap();
        let root = call.compile_time_store.as_ref().unwrap();
        let cache = root.view.lock().unwrap();
        let cached = cache.directories.values().next().unwrap();
        assert_eq!(
            listing.files.as_ptr(),
            cached.files.as_ptr(),
            "caller must share actual admitted listing, not return a plain clone"
        );
        assert_eq!(
            listing.files[0].as_os_str().as_encoded_bytes().as_ptr(),
            cached.files[0].as_os_str().as_encoded_bytes().as_ptr()
        );
    }

    #[test]
    fn candidate_scratch_paths_drop_refunds_only_temporary_paths_not_directory_report_source() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        let (dir, root, mut resolver) = physical_report_resolver();
        let entry = resolver.context.path_entry_for(dir.path()).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let source = resolver
            .include_capability(
                IncludeResolveRequest {
                    including_path: &root,
                    byte_range: 0..0,
                    requested_name: "value.inc",
                    legacy_route: None,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
        let mut call = resolver.store_call(&mut budget).unwrap();
        let listing = call
            .list_directory(dir.path(), &entry, None, &NoCancellation, false)
            .unwrap();
        let before = budget.snapshot();
        let paths = call
            .paths_for_names(
                dir.path(),
                &entry,
                &["Other.pas".into()],
                &listing,
                &NoCancellation,
            )
            .unwrap();
        assert_eq!(paths.len(), 1);
        assert!(budget.snapshot().retained_bytes > before.retained_bytes);
        let spent = budget.snapshot();
        drop(paths);
        assert_eq!(
            budget.snapshot().retained_bytes,
            before.retained_bytes,
            "temporary candidate paths must not remain billed to StoreCache"
        );
        assert_eq!(budget.snapshot().work, spent.work);
        assert_eq!(budget.snapshot().byte_work, spent.byte_work);
        assert!(matches!(source.result(), Resolution::Found(_)));
        assert!(!source.reports().observations().is_empty());
        assert!(!listing.files.is_empty());
    }

    #[test]
    fn candidate_scratch_group_borrows_original_paths_and_keeps_source_after_drop() {
        use crate::compile_time::CompileTimeLimits;
        let (dir, _, mut resolver) = directory_probe_resolver();
        let entry = resolver.context.path_entry_for(dir.path()).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut call = resolver.store_call(&mut budget).unwrap();
        let listing = call
            .list_directory(dir.path(), &entry, None, &NoCancellation, false)
            .unwrap();
        let paths = call
            .paths_for_names(
                dir.path(),
                &entry,
                &["Other.pas".into()],
                &listing,
                &NoCancellation,
            )
            .unwrap();
        let original = paths.as_ptr();
        let group = CandidateGroup {
            paths,
            legacy_route: None,
        };
        let result = call
            .resolve_candidate_group(&group, "Other", "Other", &NoCancellation)
            .unwrap()
            .unwrap();
        assert_eq!(
            group.paths.as_ptr(),
            original,
            "candidate iteration keeps the original owning group; source loading canonicalizes its own path independently"
        );
        let spent = budget.snapshot();
        drop(group);
        assert!(budget.snapshot().retained_bytes < spent.retained_bytes);
        assert_eq!(budget.snapshot().work, spent.work);
        assert_eq!(budget.snapshot().byte_work, spent.byte_work);
        assert!(
            matches!(&result,Resolution::Found(unit) if unit.declared_name=="Other" && unit.source.compile_time_backing.is_some() && !unit.source.analysis_bytes().is_empty())
        );
        assert!(!listing.files.is_empty());
        assert!(!call.report.observations.is_empty());
    }

    #[test]
    fn candidate_scratch_missing_overlay_dedup_and_case_variants_keep_real_capacity_owner() {
        use crate::compile_time::CompileTimeLimits;
        let (dir, _, mut resolver) = directory_probe_resolver();
        fs::write(dir.path().join("OTHER.pas"), "unit Other;").unwrap();
        let overlay = dir.path().join("Other.pas");
        resolver
            .store
            .inner
            .insert_overlay(&overlay, 1, Arc::<[u8]>::from(&b"unit Other;"[..]));
        let entry = resolver.context.path_entry_for(dir.path()).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let observer = budget
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut budget)
            .unwrap();
        let baseline = budget.snapshot().retained_bytes;
        let paths;
        {
            let mut call = resolver.store_call(&mut budget).unwrap();
            let listing = call
                .list_directory(dir.path(), &entry, None, &NoCancellation, false)
                .unwrap();
            let before = budget.snapshot();
            let missing = call
                .paths_for_names(
                    dir.path(),
                    &entry,
                    &["Never.pas".into()],
                    &listing,
                    &NoCancellation,
                )
                .unwrap();
            assert!(missing.is_empty());
            assert!(
                missing.capacity() >= listing.files.len(),
                "empty result still owns allocated receiving capacity until drop"
            );
            drop(missing);
            assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
            paths = call
                .paths_for_names(
                    dir.path(),
                    &entry,
                    &["Other.pas".into()],
                    &listing,
                    &NoCancellation,
                )
                .unwrap();
            assert_eq!(
                paths.iter().filter(|(p, _)| p == &overlay).count(),
                1,
                "disk+overlay identity is deduplicated without cloning the overlay buffer"
            );
            #[cfg(not(windows))]
            if !path_identity::is_case_insensitive(dir.path()) {
                assert_eq!(
                    paths.len(),
                    2,
                    "case-sensitive filesystem keeps distinct case variants"
                );
            }
            assert!(paths.iter().all(|(_, e)| e.provenance == entry.provenance));
        }
        drop(resolver);
        drop(budget);
        let live = observer.budget_snapshot();
        assert!(live.retained_bytes > baseline);
        assert!(!paths.is_empty());
        drop(paths);
        assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
        assert_eq!(observer.budget_snapshot().work, live.work);
        assert_eq!(observer.budget_snapshot().byte_work, live.byte_work);
    }

    #[test]
    fn candidate_scratch_name_tiers_preserve_order_and_refund_independently() {
        use crate::compile_time::CompileTimeLimits;
        let (_dir, _, mut resolver) = physical_report_resolver();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let call = resolver.store_call(&mut budget).unwrap();
        let namespaces = vec![" First. ".to_string(), "Second".to_string()];
        let before = budget.snapshot();
        let tiers = call
            .filename_tiers_scratch("Other", &namespaces, &NoCancellation)
            .unwrap();
        assert_eq!(
            &*tiers,
            &vec![
                vec!["Other.pas".to_string()],
                vec!["First.Other.pas".to_string()],
                vec!["Second.Other.pas".to_string()]
            ]
        );
        let spent = budget.snapshot();
        drop(tiers);
        assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
        assert_eq!(budget.snapshot().work, spent.work);
        assert_eq!(budget.snapshot().byte_work, spent.byte_work);
        let tiers = call
            .filename_tiers_scratch("First.Other", &namespaces, &NoCancellation)
            .unwrap();
        assert_eq!(
            &*tiers,
            &vec![vec!["First.Other.pas".to_string(), "Other.pas".to_string()]]
        );
        drop(tiers);
        assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
    }

    #[test]
    fn candidate_scratch_actual_alias_namespace_and_ambiguous_callbacks_keep_behavior() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        let (dir, root, mut resolver) = physical_report_resolver();
        resolver
            .context
            .unit_aliases
            .insert("Alias".into(), "Other".into());
        resolver.context.unit_namespaces = vec!["Space".into()];
        fs::write(
            dir.path().join("Space.Named.pas"),
            "unit Space.Named; interface type T=Byte; implementation end.",
        )
        .unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        for name in ["Alias", "Named"] {
            let out = resolver
                .unit_report_capability(
                    UnitResolveRequest {
                        importer_path: &root,
                        requested_name: name,
                        legacy_route: None,
                    },
                    &mut budget,
                    &NoCancellation,
                )
                .unwrap();
            assert!(
                matches!(out.result(), Resolution::Found(_)),
                "actual alias/namespace lookup {name}"
            );
            assert!(!out.reports().observations().is_empty());
            let before = budget.snapshot();
            drop(out);
            assert_eq!(budget.snapshot().work, before.work);
            assert_eq!(budget.snapshot().byte_work, before.byte_work);
        }
        if !path_identity::is_case_insensitive(dir.path()) {
            fs::write(
                dir.path().join("OTHER.pas"),
                "unit Other; interface implementation end.",
            )
            .unwrap();
            let out = resolver
                .unit_report_capability(
                    UnitResolveRequest {
                        importer_path: &root,
                        requested_name: "Alias",
                        legacy_route: None,
                    },
                    &mut budget,
                    &NoCancellation,
                )
                .unwrap();
            assert!(
                matches!(out.result(),Resolution::Ambiguous { candidates } if candidates.len()==2)
            );
            assert!(
                out.reports()
                    .warnings()
                    .iter()
                    .any(|w| w.contains("ambiguous unit"))
            );
        }
    }

    #[test]
    fn candidate_scratch_precopy_refusal_partial_cancel_error_and_unwind_refund_temporary_storage()
    {
        use crate::compile_time::CompileTimeLimits;
        for failure in [
            DirectoryFailure::Error,
            DirectoryFailure::Cancel,
            DirectoryFailure::Panic,
            DirectoryFailure::Foreign,
        ] {
            let (dir, _, mut resolver) = directory_probe_resolver();
            let entry = resolver.context.path_entry_for(dir.path()).unwrap();
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let mut call = resolver.store_call(&mut budget).unwrap();
            let listing = call
                .list_directory(dir.path(), &entry, None, &NoCancellation, false)
                .unwrap();
            call.store.overlay_failure = failure;
            let before = budget.snapshot();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                call.paths_for_names(
                    dir.path(),
                    &entry,
                    &["Other.pas".into()],
                    &listing,
                    &NoCancellation,
                )
            }));
            if matches!(failure, DirectoryFailure::Panic) {
                assert!(result.is_err());
            } else {
                assert!(result.unwrap().is_err());
            }
            assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
            assert!(budget.snapshot().work >= before.work);
            assert!(budget.snapshot().byte_work >= before.byte_work);
            assert!(!listing.files.is_empty());
        }
        struct Poll {
            count: std::sync::atomic::AtomicUsize,
            stop: usize,
        }
        impl CancellationToken for Poll {
            fn is_cancelled(&self) -> bool {
                self.count.fetch_add(1, Ordering::Relaxed) >= self.stop
            }
        }
        let mut cancellations = 0;
        let mut complete = false;
        for stop in 0..100 {
            let (dir, _, mut resolver) = directory_probe_resolver();
            let entry = resolver.context.path_entry_for(dir.path()).unwrap();
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let mut call = resolver.store_call(&mut budget).unwrap();
            let listing = call
                .list_directory(dir.path(), &entry, None, &NoCancellation, false)
                .unwrap();
            let before = budget.snapshot();
            let poll = Poll {
                count: 0.into(),
                stop,
            };
            match call.paths_for_names(dir.path(), &entry, &["Other.pas".into()], &listing, &poll) {
                Err(ResolverError::Cancelled) => {
                    cancellations += 1;
                }
                Ok(paths) => {
                    assert_eq!(paths.len(), 1);
                    drop(paths);
                    complete = true;
                }
                Err(other) => panic!("unexpected candidate error {other:?}"),
            }
            assert_eq!(
                budget.snapshot().retained_bytes,
                before.retained_bytes,
                "partial copies refund at poll {stop}"
            );
            assert!(budget.snapshot().work >= before.work);
            assert!(budget.snapshot().byte_work >= before.byte_work);
            if complete {
                break;
            }
        }
        assert!(complete);
        assert!(cancellations > 5);
        let (dir, _, mut resolver) = directory_probe_resolver();
        let entry = resolver.context.path_entry_for(dir.path()).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let listing;
        {
            let mut call = resolver.store_call(&mut budget).unwrap();
            listing = call
                .list_directory(dir.path(), &entry, None, &NoCancellation, false)
                .unwrap();
        }
        let before = budget.snapshot();
        budget
            .constrain_retained(before.retained_bytes + 1024)
            .unwrap();
        let mut call = resolver.store_call(&mut budget).unwrap();
        let original = listing.files.as_ptr();
        assert!(matches!(
            call.paths_for_names(
                dir.path(),
                &entry,
                &["Other.pas".into()],
                &listing,
                &NoCancellation
            ),
            Err(ResolverError::LimitExceeded { .. })
        ));
        assert_eq!(listing.files.as_ptr(), original);
        assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
        assert!(
            call.store.overlay_calls.get() > 0,
            "refusal still precedes candidate copying/receiving allocation after the admitted overlay read"
        );
    }

    fn physical_directory_backing_invoice(listing: &DirectoryListing, directory: &Path) -> usize {
        let count = listing.files.len() + listing.directories.len();
        std::mem::size_of::<AdmittedOwned<DirectoryListing>>()
            + 2 * std::mem::size_of::<usize>() // Arc's actual counters; this backing is usize-aligned
            + count * (directory.as_os_str().len() + 1 + if cfg!(windows) { 1024 } else { 255 })
            + (listing.files.capacity() + listing.directories.capacity()) * std::mem::size_of::<PathBuf>()
    }

    #[derive(Clone, Copy)]
    enum DirectoryFailure {
        None,
        Error,
        Cancel,
        Panic,
        Foreign,
    }
    struct DirectoryProbeStore {
        inner: FilesystemSourceStore,
        failure: DirectoryFailure,
        failure_call: Option<usize>,
        calls: usize,
        files: usize,
        overlay_failure: DirectoryFailure,
        overlay_calls: std::cell::Cell<usize>,
    }
    impl SourceStore for DirectoryProbeStore {
        fn list_directory_admitted(
            &mut self,
            r: DirectoryRequest<'_>,
            b: &mut CompileTimeBudget<'_>,
            c: &dyn CancellationToken,
        ) -> Result<AdmittedOwned<DirectoryListing>, SourceStoreError> {
            self.calls += 1;
            let listing = self.inner.list_directory_admitted(r, b, c)?;
            self.files = listing.files.as_ptr() as usize;
            if self.failure_call.is_some_and(|call| call != self.calls) {
                return Ok(listing);
            }
            match self.failure {
                DirectoryFailure::None => Ok(listing),
                DirectoryFailure::Error => Err(SourceStoreError::Io {
                    path: r.directory.into(),
                    message: "directory build completed, injected error".into(),
                }),
                DirectoryFailure::Cancel => Err(SourceStoreError::Cancelled),
                DirectoryFailure::Panic => panic!("directory build completed, injected unwind"),
                DirectoryFailure::Foreign => {
                    drop(listing);
                    let mut other = CompileTimeBudget::new(
                        crate::compile_time::CompileTimeLimits::default(),
                        &NoCancellation,
                    );
                    self.inner
                        .list_directory_admitted(r, &mut other, &NoCancellation)
                }
            }
        }
        fn overlay_candidates_admitted(
            &self,
            r: &[PathBuf],
            n: &[String],
            b: &mut CompileTimeBudget<'_>,
            c: &dyn CancellationToken,
        ) -> Result<AdmittedOwned<Vec<PathBuf>>, SourceStoreError> {
            self.overlay_calls.set(self.overlay_calls.get() + 1);
            let paths = self.inner.overlay_candidates_admitted(r, n, b, c)?;
            match self.overlay_failure {
                DirectoryFailure::None => Ok(paths),
                DirectoryFailure::Error => Err(SourceStoreError::AdmissionUnavailable),
                DirectoryFailure::Cancel => Err(SourceStoreError::Cancelled),
                DirectoryFailure::Panic => panic!("actual overlay scratch unwind after build"),
                DirectoryFailure::Foreign => {
                    drop(paths);
                    let mut other = CompileTimeBudget::new(
                        crate::compile_time::CompileTimeLimits::default(),
                        &NoCancellation,
                    );
                    self.inner
                        .overlay_candidates_admitted(r, n, &mut other, &NoCancellation)
                }
            }
        }
        fn load_admitted(
            &mut self,
            r: SourceRequest<'_>,
            b: &mut CompileTimeBudget<'_>,
            c: &dyn CancellationToken,
        ) -> Result<LoadedSource, SourceStoreError> {
            self.inner.load_admitted(r, b, c)
        }
        fn list_directory(
            &mut self,
            _: DirectoryRequest<'_>,
            _: &dyn CancellationToken,
        ) -> Result<DirectoryListing, SourceStoreError> {
            panic!("typed resolver must never use legacy directory callback")
        }
        fn overlay_candidates(&self, r: &[PathBuf], n: &[String]) -> Vec<PathBuf> {
            self.inner.overlay_candidates(r, n)
        }
        fn load(
            &mut self,
            r: SourceRequest<'_>,
            c: &dyn CancellationToken,
        ) -> Result<LoadedSource, SourceStoreError> {
            self.inner.load(r, c)
        }
    }
    fn directory_probe_resolver() -> (
        tempfile::TempDir,
        PathBuf,
        UnitResolver<DirectoryProbeStore>,
    ) {
        let (dir, root, resolver) = physical_report_resolver();
        let probe = UnitResolver::new(
            resolver.context,
            vec![dir.path().into()],
            DirectoryProbeStore {
                inner: FilesystemSourceStore::new(),
                failure: DirectoryFailure::None,
                failure_call: None,
                calls: 0,
                files: 0,
                overlay_failure: DirectoryFailure::None,
                overlay_calls: 0.into(),
            },
            Default::default(),
        );
        (dir, root, probe)
    }

    #[test]
    fn directory_ownership_store_move_freshness_overlay_and_build_failures() {
        use crate::compile_time::CompileTimeLimits;
        for failure in [
            DirectoryFailure::Error,
            DirectoryFailure::Cancel,
            DirectoryFailure::Panic,
            DirectoryFailure::Foreign,
        ] {
            for replace in [false, true] {
                let (dir, _root, mut resolver) = directory_probe_resolver();
                let entry = resolver.context.path_entry_for(dir.path()).unwrap();
                let mut budget =
                    CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
                let mut call = resolver.store_call(&mut budget).unwrap();
                let previous = if replace {
                    Some(
                        call.list_directory(dir.path(), &entry, None, &NoCancellation, false)
                            .unwrap(),
                    )
                } else {
                    None
                };
                let before = budget.snapshot();
                let capacity = call
                    .compile_time_store
                    .as_ref()
                    .unwrap()
                    .view
                    .lock()
                    .unwrap()
                    .directories
                    .capacity();
                call.store.failure = failure;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    call.list_directory(dir.path(), &entry, None, &NoCancellation, false)
                }));
                match failure {
                    DirectoryFailure::Panic => assert!(result.is_err()),
                    DirectoryFailure::Cancel => {
                        assert!(matches!(result.unwrap(), Err(ResolverError::Cancelled)))
                    }
                    DirectoryFailure::Foreign => assert!(matches!(
                        result.unwrap(),
                        Err(ResolverError::InvalidRequest(_))
                    )),
                    _ => assert!(result.unwrap().is_err()),
                }
                assert_eq!(
                    budget.snapshot().retained_bytes,
                    before.retained_bytes,
                    "failed new/replacement construction releases temporary keys/listing, not old cache/report"
                );
                assert!(budget.snapshot().work > before.work);
                assert!(budget.snapshot().byte_work > before.byte_work);
                let cache = call
                    .compile_time_store
                    .as_ref()
                    .unwrap()
                    .view
                    .lock()
                    .unwrap();
                assert_eq!(cache.directories.capacity(), capacity);
                assert_eq!(cache.directories.len(), usize::from(replace));
                if let Some(previous) = &previous {
                    assert_eq!(
                        previous.files.as_ptr(),
                        cache.directories.values().next().unwrap().files.as_ptr()
                    );
                }
            }
        }
        let (dir, _root, mut resolver) = directory_probe_resolver();
        let entry = resolver.context.path_entry_for(dir.path()).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut call = resolver.store_call(&mut budget).unwrap();
        let first = call
            .list_directory(dir.path(), &entry, None, &NoCancellation, false)
            .unwrap();
        assert_eq!(
            first.files.as_ptr() as usize,
            call.store.files,
            "store->caller handoff moves original Vec"
        );
        let overlay = dir.path().join("Unsaved.pas");
        call.store
            .inner
            .insert_overlay(&overlay, 1, Arc::<[u8]>::from(&b"unit Unsaved;"[..]));
        let second = call
            .list_directory(dir.path(), &entry, None, &NoCancellation, false)
            .unwrap();
        assert_eq!(
            call.store.calls, 2,
            "same-root cache presence does not skip freshness reread"
        );
        assert_eq!(second.files.as_ptr() as usize, call.store.files);
        assert!(!first.files.contains(&overlay));
        assert!(second.files.contains(&overlay));
        assert_eq!(
            first.stamp, second.stamp,
            "overlay freshness cannot rely only on disk stamp"
        );
        call.store.inner.insert_overlay(
            &overlay,
            2,
            Arc::<[u8]>::from(&b"unit Unsaved; // version two"[..]),
        );
        let third = call
            .list_directory(dir.path(), &entry, None, &NoCancellation, false)
            .unwrap();
        assert_eq!(
            call.store.calls, 3,
            "overlay version change still performs the required fresh listing"
        );
        assert_ne!(second.files.as_ptr(), third.files.as_ptr());
        assert_eq!(third.files.as_ptr() as usize, call.store.files);
        assert!(second.files.contains(&overlay));
        assert!(third.files.contains(&overlay));
        assert!(call.report.observations.iter().any(|o| matches!(o,ResolutionObservation::Directory { path,entry:e,.. } if path == dir.path() && e == &entry)));
    }

    #[test]
    fn directory_ownership_cancel_every_actual_construction_poll_keeps_old_listing() {
        use crate::compile_time::CompileTimeLimits;
        struct PollCancel {
            calls: std::sync::atomic::AtomicUsize,
            stop: usize,
        }
        impl CancellationToken for PollCancel {
            fn is_cancelled(&self) -> bool {
                self.calls.fetch_add(1, Ordering::Relaxed) >= self.stop
            }
        }
        let mut cancellations = 0;
        let mut successful = false;
        for stop in 0..180 {
            let (dir, _root, mut resolver) = directory_probe_resolver();
            let entry = resolver.context.path_entry_for(dir.path()).unwrap();
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let mut call = resolver.store_call(&mut budget).unwrap();
            let previous = call
                .list_directory(dir.path(), &entry, None, &NoCancellation, false)
                .unwrap();
            let before = budget.snapshot();
            let cancel = PollCancel {
                calls: 0.into(),
                stop,
            };
            match call.list_directory(dir.path(), &entry, None, &cancel, false) {
                Err(ResolverError::Cancelled) => {
                    cancellations += 1;
                    let cache = call
                        .compile_time_store
                        .as_ref()
                        .unwrap()
                        .view
                        .lock()
                        .unwrap();
                    assert_eq!(
                        cache.directories.values().next().unwrap().files.as_ptr(),
                        previous.files.as_ptr()
                    );
                    assert_eq!(
                        budget.snapshot().retained_bytes,
                        before.retained_bytes,
                        "partial replacement storage refunds at poll {stop}"
                    );
                    assert!(budget.snapshot().work >= before.work);
                    assert!(budget.snapshot().byte_work >= before.byte_work);
                }
                Ok(next) => {
                    assert_eq!(next.files, previous.files);
                    successful = true;
                    break;
                }
                Err(other) => panic!("unexpected replacement failure: {other:?}"),
            }
        }
        assert!(cancellations > 10);
        assert!(successful);
    }

    #[test]
    fn directory_ownership_low_budget_prekey_and_peak_map_refusal() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        let (dir, _root, mut resolver) = directory_probe_resolver();
        let entry = resolver.context.path_entry_for(dir.path()).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        {
            let mut call = resolver.store_call(&mut budget).unwrap();
            let first = call
                .list_directory(dir.path(), &entry, None, &NoCancellation, false)
                .unwrap();
            drop(first);
        }
        let before = budget.snapshot();
        budget.constrain_retained(before.retained_bytes).unwrap();
        {
            let mut call = resolver.store_call(&mut budget).unwrap();
            let calls = call.store.calls;
            assert!(matches!(
                call.list_directory(dir.path(), &entry, None, &NoCancellation, false),
                Err(ResolverError::LimitExceeded { .. })
            ));
            assert_eq!(
                call.store.calls, calls,
                "no store/copy reached before canonical/key admission refusal"
            );
        }
        assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
        let (key_dir, _, mut key_resolver) = directory_probe_resolver();
        let key_entry = key_resolver.context.path_entry_for(key_dir.path()).unwrap();
        let mut key_budget = CompileTimeBudget::new(
            CompileTimeLimits {
                max_byte_work: 4 * key_dir.path().as_os_str().len(),
                ..CompileTimeLimits::default()
            },
            &NoCancellation,
        );
        let mut key_call = key_resolver.store_call(&mut key_budget).unwrap();
        let key_before = key_budget.snapshot();
        assert!(matches!(
            key_call.list_directory(key_dir.path(), &key_entry, None, &NoCancellation, false),
            Err(ResolverError::LimitExceeded {
                limit: "max_byte_work",
                ..
            })
        ));
        assert_eq!(
            key_call.store.calls, 0,
            "key copy is refused after canonicalization, before entering the filesystem store"
        );
        assert_eq!(
            key_budget.snapshot().retained_bytes,
            key_before.retained_bytes
        );
        assert_eq!(
            key_budget.snapshot().byte_work,
            4 * key_dir.path().as_os_str().len()
        );
        // Use a real admitted filesystem listing and exact construction-owned
        // key from the actual cache. Empty-map growth must refuse before any
        // receiving allocation; no old capacity/key/handle invoice is released.
        let (dir, root, mut resolver) = directory_probe_resolver();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let out = resolver
            .include_capability(
                IncludeResolveRequest {
                    including_path: &root,
                    byte_range: 0..0,
                    requested_name: "value.inc",
                    legacy_route: None,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
        let entry = resolver.context.path_entry_for(dir.path()).unwrap();
        let mut call = resolver.store_call(&mut budget).unwrap();
        let caller = call
            .list_directory(dir.path(), &entry, None, &NoCancellation, false)
            .unwrap();
        let root_cache = call.compile_time_store.as_ref().unwrap().clone();
        let mut cache = root_cache.view.lock().unwrap();
        let (key, listing) = cache.value_mut().directories.drain().next().unwrap();
        let capacity = cache.directories.capacity();
        let before = budget.snapshot();
        let mut receiving = StoreCache::default();
        budget.constrain_retained(before.retained_bytes).unwrap();
        assert!(matches!(
            receiving.insert_directory(key, listing, &mut budget),
            Err(ResolverError::LimitExceeded { .. })
        ));
        assert_eq!(receiving.directories.capacity(), 0);
        assert!(receiving.directory_capacity.is_none());
        assert_eq!(cache.directories.capacity(), capacity);
        assert!(cache.directory_capacity.is_some());
        assert!(call.report.observations.iter().any(
            |o| matches!(o,ResolutionObservation::Directory { path,.. } if path == dir.path())
        ));
        assert_eq!(caller.files.as_ptr() as usize, call.store.files);
        assert!(matches!(out.result(), Resolution::Found(_)));
        // Keep the actual resolver call live to prove the cache lock is local,
        // not part of a returned directory capability.
        drop(cache);
        drop(call);
    }

    #[test]
    fn directory_ownership_replacement_and_last_owner_refund_with_live_report_source() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        let (dir, root, mut resolver) = physical_report_resolver();
        let entry = resolver.context.path_entry_for(dir.path()).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let out = resolver
            .include_capability(
                IncludeResolveRequest {
                    including_path: &root,
                    byte_range: 0..0,
                    requested_name: "value.inc",
                    legacy_route: None,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
        let (source, reports) = out.into_parts();
        let mut call = resolver.store_call(&mut budget).unwrap();
        let first = call
            .list_directory(dir.path(), &entry, None, &NoCancellation, false)
            .unwrap();
        let old_pointer = first.files.as_ptr();
        let old_invoice = physical_directory_backing_invoice(&first, dir.path());
        fs::write(dir.path().join("Added.pas"), "unit Added;").unwrap();
        let second = call
            .list_directory(dir.path(), &entry, None, &NoCancellation, false)
            .unwrap();
        assert_ne!(
            second.files.as_ptr(),
            old_pointer,
            "deliberate reread replaces the cache allocation"
        );
        assert!(!first.files.iter().any(|p| p.ends_with("Added.pas")));
        assert!(second.files.iter().any(|p| p.ends_with("Added.pas")));
        let before = budget.snapshot();
        drop(first);
        assert_eq!(
            before.retained_bytes - budget.snapshot().retained_bytes,
            old_invoice + std::mem::size_of::<AdmittedShared<DirectoryListing>>(),
            "old caller was last owner after replacement"
        );
        assert_eq!(before.work, budget.snapshot().work);
        assert_eq!(before.byte_work, budget.snapshot().byte_work);
        let new_pointer = second.files.as_ptr();
        let new_invoice = physical_directory_backing_invoice(&second, dir.path());
        let before = budget.snapshot();
        drop(second);
        assert_eq!(
            before.retained_bytes - budget.snapshot().retained_bytes,
            std::mem::size_of::<AdmittedShared<DirectoryListing>>(),
            "cache keeps backing alive after caller drop"
        );
        let root_cache = call.compile_time_store.as_ref().unwrap().clone();
        let mut cache = root_cache.view.lock().unwrap();
        let cached = cache.directories.values().next().unwrap();
        assert_eq!(cached.files.as_ptr(), new_pointer);
        let last = cached.try_clone(&mut budget).unwrap();
        let map_capacity = cache.directories.capacity();
        let key = cache.directories.keys().next().unwrap();
        let mut key_payload = key.0.path.as_os_str().len() * 4;
        key.0
            .entry
            .visit_recovery_payload(&mut |n| {
                key_payload += n;
                Ok(())
            })
            .unwrap();
        key.0
            .read_policy
            .visit_owned_clone_payload(&mut |n| {
                key_payload += n;
                Ok(())
            })
            .unwrap();
        let before = budget.snapshot();
        let entries = cache.value_mut().directories.drain().collect::<Vec<_>>();
        drop(entries);
        assert_eq!(
            before.retained_bytes - budget.snapshot().retained_bytes,
            key_payload
                + std::mem::size_of::<AdmittedOwned<DirectoryCacheKey>>()
                + std::mem::size_of::<AdmittedShared<DirectoryListing>>()
        );
        assert_eq!(cache.directories.capacity(), map_capacity);
        assert!(
            cache.directory_capacity.is_some(),
            "empty live map keeps its independent backing invoice"
        );
        assert_eq!(last.files.as_ptr(), new_pointer);
        drop(cache);
        let before = budget.snapshot();
        drop(last);
        assert_eq!(
            before.retained_bytes - budget.snapshot().retained_bytes,
            new_invoice + std::mem::size_of::<AdmittedShared<DirectoryListing>>()
        );
        assert_eq!(before.work, budget.snapshot().work);
        assert_eq!(before.byte_work, budget.snapshot().byte_work);
        let mut cache = root_cache.view.lock().unwrap();
        let before = budget.snapshot();
        let backing = std::mem::take(&mut cache.value_mut().directories);
        let capacity_owner = cache.value_mut().directory_capacity.take().unwrap();
        drop(backing);
        drop(capacity_owner);
        let capacity_invoice = std::mem::size_of::<AdmittedOwned<()>>()
            + 4 * (std::mem::size_of::<(OwnedDirectoryKey, AdmittedShared<DirectoryListing>)>()
                + 1)
            + 16;
        assert_eq!(
            before.retained_bytes - budget.snapshot().retained_bytes,
            capacity_invoice,
            "only the now-dead empty directory map's exact minted bound retires"
        );
        assert_eq!(before.work, budget.snapshot().work);
        assert_eq!(before.byte_work, budget.snapshot().byte_work);
        drop(cache);
        assert!(!reports.observations().is_empty());
        assert!(matches!(source.result(),Resolution::Found(s) if !s.analysis_bytes().is_empty()));
    }

    #[test]
    fn directory_ownership_context_root_missing_and_survival_are_independent() {
        use crate::compile_time::CompileTimeLimits;
        let (dir, _root, mut resolver) = physical_report_resolver();
        let entry = resolver.context.path_entry_for(dir.path()).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let observer = budget
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut budget)
            .unwrap();
        let baseline = budget.snapshot().retained_bytes;
        let first;
        let first_invoice;
        {
            let mut call = resolver.store_call(&mut budget).unwrap();
            first = call
                .list_directory(dir.path(), &entry, None, &NoCancellation, false)
                .unwrap();
            first_invoice = physical_directory_backing_invoice(&first, dir.path());
            let alternate = ProjectPathEntry {
                path: entry.path.clone(),
                provenance: ProjectPathProvenance::Configured,
            };
            let second = call
                .list_directory(dir.path(), &alternate, None, &NoCancellation, false)
                .unwrap();
            assert_ne!(second.files.as_ptr(), first.files.as_ptr());
            let root_cache = call.compile_time_store.as_ref().unwrap();
            assert_eq!(
                root_cache.view.lock().unwrap().directories.len(),
                2,
                "authorization entry is part of cache identity"
            );
            let previous_policy = call.context.read_policy.clone();
            call.context.read_policy = ReadPolicy::new(
                &[dir.path().into()],
                &[],
                &["**/*.denied".into()],
                &Default::default(),
            );
            let policy_listing = call
                .list_directory(dir.path(), &entry, None, &NoCancellation, false)
                .unwrap();
            assert_ne!(policy_listing.files.as_ptr(), first.files.as_ptr());
            {
                let cache = call
                    .compile_time_store
                    .as_ref()
                    .unwrap()
                    .view
                    .lock()
                    .unwrap();
                assert_eq!(
                    cache.directories.len(),
                    3,
                    "same path/entry with a different read policy has an independent key"
                );
                assert!(
                    cache
                        .directories
                        .keys()
                        .any(|k| k.0.read_policy == previous_policy)
                );
                assert!(
                    cache
                        .directories
                        .keys()
                        .any(|k| k.0.read_policy == call.context.read_policy)
                );
            }
            call.context.read_policy = previous_policy;
            let missing = dir.path().join("missing");
            let missing_entry = ProjectPathEntry {
                path: missing.clone(),
                provenance: entry.provenance.clone(),
            };
            let optional = call
                .list_directory(&missing, &missing_entry, None, &NoCancellation, false)
                .unwrap();
            let required = call
                .list_directory(&missing, &missing_entry, None, &NoCancellation, true)
                .unwrap();
            assert!(optional.complete);
            assert!(!required.complete);
            assert!(required.files.is_empty());
            assert!(call.report.observations.iter().any(|o| matches!(o,ResolutionObservation::Directory { path,complete:false,.. } if path == &missing)));
        }
        // Another root must neither reuse the original root's backing nor mint
        // its handle through the wrong budget identity.
        let mut other = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        if let DirectoryAccess::Admitted(first_shared) = &first {
            let before = other.snapshot();
            assert!(first_shared.try_clone(&mut other).is_err());
            assert_eq!(other.snapshot(), before);
        } else {
            panic!("typed listing")
        }
        {
            let mut call = resolver.store_call(&mut other).unwrap();
            let second = call
                .list_directory(dir.path(), &entry, None, &NoCancellation, false)
                .unwrap();
            assert_ne!(second.files.as_ptr(), first.files.as_ptr());
        }
        drop(resolver);
        drop(budget);
        drop(other);
        let live = observer.budget_snapshot();
        assert_eq!(
            live.retained_bytes,
            baseline + first_invoice + std::mem::size_of::<AdmittedShared<DirectoryListing>>()
        );
        assert!(!first.files.is_empty());
        drop(first);
        assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
        assert_eq!(observer.budget_snapshot().work, live.work);
        assert_eq!(observer.budget_snapshot().byte_work, live.byte_work);
    }

    #[test]
    fn directory_ownership_actual_map_growth_budget_refusals_preserve_old_backing() {
        use crate::compile_time::CompileTimeLimits;
        let mut grew = false;
        for extra in (0..16384).step_by(32) {
            let (dir, _, mut resolver) = directory_probe_resolver();
            let base_entry = resolver.context.path_entry_for(dir.path()).unwrap();
            let paths = (0..4)
                .map(|n| dir.path().join(format!("D{n}")))
                .collect::<Vec<_>>();
            for path in &paths {
                fs::create_dir(path).unwrap();
                fs::write(path.join("One.pas"), "unit One;").unwrap();
            }
            let entries = paths
                .iter()
                .map(|p| ProjectPathEntry {
                    path: p.clone(),
                    provenance: base_entry.provenance.clone(),
                })
                .collect::<Vec<_>>();
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let mut callers = Vec::new();
            {
                let mut call = resolver.store_call(&mut budget).unwrap();
                for i in 0..3 {
                    callers.push(
                        call.list_directory(&paths[i], &entries[i], None, &NoCancellation, false)
                            .unwrap(),
                    );
                }
            }
            let before = budget.snapshot();
            budget
                .constrain_retained(before.retained_bytes + extra)
                .unwrap();
            let mut call = resolver.store_call(&mut budget).unwrap();
            let root_cache = call.compile_time_store.as_ref().unwrap().clone();
            let old_capacity = root_cache.view.lock().unwrap().directories.capacity();
            assert_eq!(old_capacity, 3);
            match call.list_directory(&paths[3], &entries[3], None, &NoCancellation, false) {
                Err(ResolverError::LimitExceeded { .. }) => {
                    let cache = root_cache.view.lock().unwrap();
                    assert_eq!(cache.directories.len(), 3);
                    assert_eq!(cache.directories.capacity(), old_capacity);
                    assert!(cache.directory_capacity.is_some());
                    for caller in &callers {
                        assert!(
                            cache
                                .directories
                                .values()
                                .any(|v| v.files.as_ptr() == caller.files.as_ptr())
                        );
                    }
                    assert!(budget.snapshot().work >= before.work);
                    assert!(budget.snapshot().byte_work >= before.byte_work);
                }
                Ok(caller) => {
                    callers.push(caller);
                    let mut cache = root_cache.view.lock().unwrap();
                    assert_eq!(cache.directories.len(), 4);
                    assert_eq!(cache.directories.capacity(), 7);
                    for caller in &callers {
                        assert!(
                            cache
                                .directories
                                .values()
                                .any(|v| v.files.as_ptr() == caller.files.as_ptr())
                        );
                    }
                    let pairs = cache.value_mut().directories.drain().collect::<Vec<_>>();
                    drop(pairs);
                    assert_eq!(cache.directories.capacity(), 7);
                    let before = budget.snapshot();
                    let map = std::mem::take(&mut cache.value_mut().directories);
                    let capacity = cache.value_mut().directory_capacity.take().unwrap();
                    drop(map);
                    drop(capacity);
                    let new_invoice = std::mem::size_of::<AdmittedOwned<()>>()
                        + 8 * (std::mem::size_of::<(
                            OwnedDirectoryKey,
                            AdmittedShared<DirectoryListing>,
                        )>() + 1)
                        + 16;
                    assert_eq!(
                        before.retained_bytes - budget.snapshot().retained_bytes,
                        new_invoice
                    );
                    assert_eq!(before.work, budget.snapshot().work);
                    assert_eq!(before.byte_work, budget.snapshot().byte_work);
                    assert!(
                        callers.iter().all(|v| v.files[0].ends_with("One.pas")),
                        "callers survive entry and map backing death"
                    );
                    grew = true;
                    break;
                }
                Err(other) => panic!("unexpected growth failure: {other:?}"),
            }
        }
        assert!(grew);
    }

    #[test]
    fn directory_ownership_nonempty_receiving_map_refuses_before_old_backing_dies() {
        use crate::compile_time::CompileTimeLimits;
        let (dir, _, mut resolver) = directory_probe_resolver();
        let entry = resolver.context.path_entry_for(dir.path()).unwrap();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut callers = Vec::new();
        let mut call = resolver.store_call(&mut budget).unwrap();
        for n in 0..4 {
            let path = dir.path().join(format!("Map{n}"));
            fs::create_dir(&path).unwrap();
            fs::write(path.join("P.pas"), "unit P;").unwrap();
            let local = ProjectPathEntry {
                path: path.clone(),
                provenance: entry.provenance.clone(),
            };
            callers.push(
                call.list_directory(&path, &local, None, &NoCancellation, false)
                    .unwrap(),
            );
        }
        let root = call.compile_time_store.as_ref().unwrap().clone();
        let mut cache = root.view.lock().unwrap();
        let mut pairs = cache.value_mut().directories.drain().collect::<Vec<_>>();
        // Test the actual receiving helper with filesystem-produced keys and
        // handles, never synthetic backing receipts or payload-sized refunds.
        let mut receiving = budget.admit_owned(0, 0, 0, StoreCache::default).unwrap();
        for _ in 0..3 {
            let (key, listing) = pairs.pop().unwrap();
            receiving
                .value_mut()
                .insert_directory(key, listing, &mut budget)
                .unwrap();
        }
        assert_eq!(receiving.directories.capacity(), 3);
        let before = budget.snapshot();
        budget.constrain_retained(before.retained_bytes).unwrap();
        let (key, listing) = pairs.pop().unwrap();
        assert!(matches!(
            receiving
                .value_mut()
                .insert_directory(key, listing, &mut budget),
            Err(ResolverError::LimitExceeded { .. })
        ));
        assert_eq!(receiving.directories.len(), 3);
        assert_eq!(receiving.directories.capacity(), 3);
        assert!(receiving.directory_capacity.is_some());
        for listing in receiving.directories.values() {
            assert!(
                callers
                    .iter()
                    .any(|caller| caller.files.as_ptr() == listing.files.as_ptr())
            );
        }
        assert!(
            callers
                .iter()
                .all(|caller| caller.files[0].ends_with("P.pas"))
        );
        assert_eq!(
            cache.directories.capacity(),
            7,
            "entry removal doesn't retire original cache table either"
        );
        let reports = call.report.observations.as_ptr();
        drop(cache);
        let before = budget.snapshot();
        let pairs = receiving
            .value_mut()
            .directories
            .drain()
            .collect::<Vec<_>>();
        drop(pairs);
        let after_entries = budget.snapshot();
        assert_eq!(receiving.directories.capacity(), 3);
        assert!(receiving.directory_capacity.is_some());
        assert_eq!(after_entries.work, before.work);
        assert_eq!(after_entries.byte_work, before.byte_work);
        let map = std::mem::take(&mut receiving.value_mut().directories);
        let owner = receiving.value_mut().directory_capacity.take().unwrap();
        drop(map);
        drop(owner);
        let invoice = std::mem::size_of::<AdmittedOwned<()>>()
            + 4 * (std::mem::size_of::<(OwnedDirectoryKey, AdmittedShared<DirectoryListing>)>()
                + 1)
            + 16;
        assert_eq!(
            after_entries.retained_bytes - budget.snapshot().retained_bytes,
            invoice
        );
        assert_eq!(
            call.report.observations.as_ptr(),
            reports,
            "map capacity refund does not touch live report storage"
        );
    }

    #[test]
    fn directory_ownership_unsupported_store_does_not_call_legacy_listing() {
        struct LegacyDirectoryStore;
        impl SourceStore for LegacyDirectoryStore {
            fn list_directory(
                &mut self,
                _: DirectoryRequest<'_>,
                _: &dyn CancellationToken,
            ) -> Result<DirectoryListing, SourceStoreError> {
                panic!("unbounded legacy directory callback must not run")
            }
            fn overlay_candidates(&self, _: &[PathBuf], _: &[String]) -> Vec<PathBuf> {
                panic!("unbounded legacy overlays must not run")
            }
            fn load(
                &mut self,
                _: SourceRequest<'_>,
                _: &dyn CancellationToken,
            ) -> Result<LoadedSource, SourceStoreError> {
                panic!("unbounded legacy load must not run")
            }
        }
        let (dir, _, original) = physical_report_resolver();
        let entry = original.context.path_entry_for(dir.path()).unwrap();
        let mut resolver = UnitResolver::new(
            original.context,
            vec![dir.path().into()],
            LegacyDirectoryStore,
            Default::default(),
        );
        let mut budget = CompileTimeBudget::new(
            crate::compile_time::CompileTimeLimits::default(),
            &NoCancellation,
        );
        let mut call = resolver.store_call(&mut budget).unwrap();
        let before = budget.snapshot();
        assert!(matches!(
            call.list_directory(dir.path(), &entry, None, &NoCancellation, false),
            Err(ResolverError::SourceStore(
                SourceStoreError::AdmissionUnavailable
            ))
        ));
        assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
        assert!(
            call.compile_time_store
                .as_ref()
                .unwrap()
                .view
                .lock()
                .unwrap()
                .directories
                .is_empty()
        );
    }

    #[test]
    fn persistent_report_typed_finish_moves_and_refunds_without_source_refund() {
        use crate::compile_time::{CompileTimeBudget, CompileTimeLimits, CompileTimeResolver};
        let (_dir, root, mut resolver) = physical_report_resolver();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let observer = budget
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut budget)
            .unwrap();
        let baseline = observer.budget_snapshot().retained_bytes;
        let outcome = CompileTimeResolver::include(
            &mut resolver,
            IncludeResolveRequest {
                including_path: &root,
                byte_range: 0..0,
                requested_name: "value.inc",
                legacy_route: None,
            },
            &mut budget,
            &NoCancellation,
        )
        .unwrap();
        assert!(matches!(outcome.result, Resolution::Found(_)));
        let vector = resolver.report.observations.as_ptr();
        let path = match resolver.report.observations.last().unwrap() {
            ResolutionObservation::Payload { path, .. } => {
                path.as_os_str().as_encoded_bytes().as_ptr()
            }
            _ => panic!("physical payload observation"),
        };
        let byte_work = budget.snapshot().byte_work;
        let report = resolver.finish_admitted(&mut budget).unwrap();
        assert_eq!(report.report().observations.as_ptr(), vector);
        let projected = report.into_report_observations(&mut budget).unwrap();
        assert_eq!(projected.observations().as_ptr(), vector);
        assert!(
            matches!(projected.observations().last(), Some(ResolutionObservation::Payload { path: p, .. }) if p.as_os_str().as_encoded_bytes().as_ptr() == path)
        );
        assert_eq!(
            budget.snapshot().byte_work,
            byte_work,
            "finish/extraction moves, not copies"
        );
        drop(budget);
        let live = observer.budget_snapshot();
        drop(projected);
        let source_only = observer.budget_snapshot();
        assert!(source_only.retained_bytes < live.retained_bytes);
        assert!(
            source_only.retained_bytes > baseline,
            "returned source owns independent backing"
        );
        assert_eq!(source_only.work, live.work);
        assert_eq!(source_only.byte_work, live.byte_work);
        drop(outcome);
        assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
    }

    #[test]
    fn returned_report_physical_callbacks_own_copies_and_transfer_without_copy_work() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        for unit in [false, true] {
            let (_dir, root, mut resolver) = physical_report_resolver();
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let observer = budget
                .admit_owned(0, 0, 0, || ())
                .unwrap()
                .into_shared(&mut budget)
                .unwrap();
            let baseline = budget.snapshot().retained_bytes;
            let reports = if unit {
                let outcome = CompileTimeResolver::unit_report_capability(
                    &mut resolver,
                    UnitResolveRequest {
                        importer_path: &root,
                        requested_name: "Other",
                        legacy_route: None,
                    },
                    &mut budget,
                    &NoCancellation,
                )
                .unwrap();
                let (result, reports) = outcome.into_parts();
                assert!(matches!(result.result(), Resolution::Found(_)));
                drop(result);
                reports
            } else {
                let outcome = CompileTimeResolver::include_capability(
                    &mut resolver,
                    IncludeResolveRequest {
                        including_path: &root,
                        byte_range: 0..0,
                        requested_name: "value.inc",
                        legacy_route: None,
                    },
                    &mut budget,
                    &NoCancellation,
                )
                .unwrap();
                let (result, reports) = outcome.into_parts();
                assert!(matches!(result.result(), Resolution::Found(_)));
                drop(result);
                reports
            };
            assert!(!reports.observations().is_empty());
            assert_ne!(
                reports.observations().as_ptr(),
                resolver.report.observations.as_ptr(),
                "legitimate independent callback copy"
            );
            let vector = reports.observations().as_ptr();
            let path = match reports.observations().last().unwrap() {
                ResolutionObservation::Payload { path, .. } => {
                    path.as_os_str().as_encoded_bytes().as_ptr()
                }
                _ => panic!("physical callback payload"),
            };
            let mut observations = budget.admit_owned(0, 0, 0, Vec::new).unwrap();
            let mut warnings = budget.admit_owned(0, 0, 0, Vec::new).unwrap();
            let before = budget.snapshot();
            reports
                .consume_into(&mut observations, &mut warnings, &mut budget)
                .unwrap();
            assert_eq!(observations.as_ptr(), vector);
            assert!(
                matches!(observations.last(), Some(ResolutionObservation::Payload { path: p, .. }) if p.as_os_str().as_encoded_bytes().as_ptr() == path)
            );
            assert_eq!(budget.snapshot().byte_work, before.byte_work);
            drop(resolver);
            drop(budget);
            let live = observer.budget_snapshot();
            assert!(live.retained_bytes > baseline);
            drop(observations);
            drop(warnings);
            assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
            assert_eq!(observer.budget_snapshot().work, live.work);
            assert_eq!(observer.budget_snapshot().byte_work, live.byte_work);
        }
    }

    #[test]
    fn returned_report_actual_root_and_provider_engine_preserve_callback_backing() {
        use crate::compile_time::{
            AdmittedProviderSource, CompileTimeLimits, CompileTimeResolver, ProviderSource,
        };
        struct Recording {
            inner: UnitResolver<FilesystemSourceStore>,
            observation: Option<usize>,
            path: Option<usize>,
            warning: Option<usize>,
        }
        impl Recording {
            fn capture(&mut self, reports: &AdmittedCapabilityReports) {
                if !reports.observations().is_empty() {
                    self.observation = Some(reports.observations().as_ptr() as usize);
                    self.path = reports.observations().iter().find_map(|o| match o {
                        ResolutionObservation::Payload { path, .. }
                        | ResolutionObservation::Candidate { path, .. } => {
                            Some(path.as_os_str().as_encoded_bytes().as_ptr() as usize)
                        }
                        _ => None,
                    });
                }
                self.warning = reports.warnings().first().map(|s| s.as_ptr() as usize);
            }
        }
        impl CompileTimeResolver for Recording {
            fn include_capability(
                &mut self,
                r: IncludeResolveRequest<'_>,
                b: &mut CompileTimeBudget<'_>,
                c: &dyn CancellationToken,
            ) -> Result<AdmittedCapabilityOutcome<LoadedSource>, ResolverError> {
                let out = self.inner.include_capability(r, b, c)?;
                self.capture(out.reports());
                Ok(out)
            }
            fn unit_report_capability(
                &mut self,
                r: UnitResolveRequest<'_>,
                b: &mut CompileTimeBudget<'_>,
                c: &dyn CancellationToken,
            ) -> Result<AdmittedCapabilityOutcome<AdmittedProviderSource>, ResolverError>
            {
                let out = self.inner.unit_report_capability(r, b, c)?;
                self.capture(out.reports());
                Ok(out)
            }
            fn include(
                &mut self,
                r: IncludeResolveRequest<'_>,
                b: &mut CompileTimeBudget<'_>,
                c: &dyn CancellationToken,
            ) -> Result<ResolutionOutcome<LoadedSource>, ResolverError> {
                self.inner.include(r, b, c)
            }
            fn unit(
                &mut self,
                r: UnitResolveRequest<'_>,
                b: &mut CompileTimeBudget<'_>,
                c: &dyn CancellationToken,
            ) -> Result<ResolutionOutcome<ProviderSource>, ResolverError> {
                self.inner.unit(r, b, c)
            }
        }
        for scenario in 0..3 {
            let unit = scenario != 0;
            let nested = scenario == 2;
            let (dir, root_path, mut inner) = physical_report_resolver();
            let text = if unit {
                "unit Main; interface uses Other; {$IF SizeOf(Other.T)=8}{$DEFINE HIT}{$ENDIF} implementation end."
            } else {
                "unit Main; interface {$I bad.inc} implementation end."
            };
            fs::write(&root_path, text).unwrap();
            fs::write(
                dir.path().join("Other.pas"),
                "unit Other; interface type T=Int64; implementation end.",
            )
            .unwrap();
            if nested {
                fs::write(
                    dir.path().join("Other.pas"),
                    "unit Other; interface {$I inside.inc} implementation end.",
                )
                .unwrap();
                fs::write(dir.path().join("inside.inc"), "type T=Int64;").unwrap();
            }
            fs::create_dir(dir.path().join("bad.inc")).unwrap();
            inner.context.conditional_context = pascal_project::ConditionalContext::default()
                .with_compiler_version(pascal_project::CompilerVersion::new(37, 0))
                .with_layout(pascal_project::LayoutContext {
                    platform: Some(pascal_project::LayoutPlatform::Win32),
                    defaults: Default::default(),
                });
            inner.context.library_conditional_context = inner.context.conditional_context.clone();
            let context = inner.context.conditional_context.clone();
            let root = inner
                .load_source(&root_path, None, SourceKind::Unit, &NoCancellation)
                .unwrap();
            let mut recording = Recording {
                inner,
                observation: None,
                path: None,
                warning: None,
            };
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
            let observer = budget
                .admit_owned(0, 0, 0, || ())
                .unwrap()
                .into_shared(&mut budget)
                .unwrap();
            let baseline = budget.snapshot().retained_bytes;
            let analysis = crate::compile_time::analyze_source_in_session(
                &root,
                &context,
                &mut recording,
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
            if !nested {
                assert_eq!(
                    Some(analysis.observations.as_ptr() as usize),
                    recording.observation
                );
            }
            assert!(analysis.observations.iter().any(|o| match o {
                ResolutionObservation::Payload { path, .. }
                | ResolutionObservation::Candidate { path, .. } =>
                    Some(path.as_os_str().as_encoded_bytes().as_ptr() as usize) == recording.path,
                _ => false,
            }));
            if unit {
                assert!(analysis.root.directives.iter().any(
                    |d| d.body == "DEFINE HIT" && d.activity == crate::conditional::Truth::True
                ));
            } else {
                assert_eq!(
                    analysis.warnings.first().map(|s| s.as_ptr() as usize),
                    recording.warning
                );
                assert!(!analysis.warnings.is_empty());
            }
            drop(root);
            drop(recording);
            drop(budget);
            let live = observer.budget_snapshot();
            assert!(live.retained_bytes > baseline);
            drop(analysis);
            assert_eq!(observer.budget_snapshot().retained_bytes, baseline);
            assert_eq!(observer.budget_snapshot().work, live.work);
            assert_eq!(observer.budget_snapshot().byte_work, live.byte_work);
        }
    }

    #[test]
    fn returned_report_drop_and_dedup_refund_named_invoices_not_persistent_or_source() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        fn invoice<T>(
            field: &AdmittedOwned<CapabilityReportField<T>>,
            body: impl Fn(&T) -> usize,
        ) -> usize {
            std::mem::size_of::<AdmittedOwned<CapabilityReportField<T>>>()
                + field.owners.capacity.as_ref().map_or(0, |_| {
                    std::mem::size_of::<ReportCredit>()
                        + field.values.capacity() * std::mem::size_of::<T>()
                })
                + field.owners.receipt_capacity.as_ref().map_or(0, |_| {
                    std::mem::size_of::<ReportCredit>()
                        + field.owners.payloads.capacity()
                            * std::mem::size_of::<Option<ReportCredit>>()
                })
                + field
                    .values
                    .iter()
                    .map(|v| std::mem::size_of::<ReportCredit>() + body(v))
                    .sum::<usize>()
        }
        let (_dir, root, mut resolver) = physical_report_resolver();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let outcome = resolver
            .include_capability(
                IncludeResolveRequest {
                    including_path: &root,
                    byte_range: 0..0,
                    requested_name: "value.inc",
                    legacy_route: None,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap();
        let (source, reports) = outcome.into_parts();
        let receipt = invoice(&reports.observations, |o| {
            crate::compile_time::observation_payload(o).unwrap()
                - std::mem::size_of::<ResolutionObservation>()
        }) + invoice(&reports.warnings, String::len);
        let before = budget.snapshot();
        let persistent = resolver.report.observations.as_ptr();
        drop(reports);
        assert_eq!(
            before.retained_bytes - budget.snapshot().retained_bytes,
            receipt
        );
        assert_eq!(before.work, budget.snapshot().work);
        assert_eq!(before.byte_work, budget.snapshot().byte_work);
        assert_eq!(resolver.report.observations.as_ptr(), persistent);
        let Resolution::Found(source) = source.into_unmigrated().unwrap() else {
            panic!("physical returned source")
        };
        assert!(source.analysis_bytes().starts_with(b"{physical"));
        let observations = resolver.report.observations.clone(); // caller-owned legacy input to explicit copy constructor
        let mut target = budget.admit_owned(0, 0, 0, Vec::new).unwrap();
        let mut warnings = budget.admit_owned(0, 0, 0, Vec::new).unwrap();
        let first = AdmittedCapabilityReports::copy(
            &observations,
            &["physical warning".into()],
            &mut budget,
        )
        .unwrap();
        first
            .consume_into(&mut target, &mut warnings, &mut budget)
            .unwrap();
        let pointers = (target.as_ptr(), warnings.as_ptr(), warnings[0].as_ptr());
        let retained = budget.snapshot().retained_bytes;
        let second = AdmittedCapabilityReports::copy(
            &observations,
            &["physical warning".into()],
            &mut budget,
        )
        .unwrap();
        let spent = budget.snapshot();
        second
            .consume_into(&mut target, &mut warnings, &mut budget)
            .unwrap();
        assert_eq!(
            budget.snapshot().retained_bytes,
            retained,
            "duplicate payload/temporary buffers die; destination capacity stays"
        );
        assert_eq!(
            (target.as_ptr(), warnings.as_ptr(), warnings[0].as_ptr()),
            pointers
        );
        assert_eq!(
            budget.snapshot().byte_work,
            spent.byte_work,
            "dedup transfer adds no copy work"
        );
        assert!(budget.snapshot().work >= spent.work);
        // Mix duplicate and genuinely new reports to exercise receiving growth.
        let mixed = AdmittedCapabilityReports::copy(
            &observations,
            &["physical warning".into(), "new warning".into()],
            &mut budget,
        )
        .unwrap();
        let new_pointer = mixed.warnings()[1].as_ptr();
        mixed
            .consume_into(&mut target, &mut warnings, &mut budget)
            .unwrap();
        assert_eq!(warnings.len(), 2);
        assert_eq!(warnings[1].as_ptr(), new_pointer);
        assert_eq!(warnings[0].as_ptr(), pointers.2);
    }

    #[test]
    fn returned_report_precopy_refusal_partial_cancel_unwind_and_foreign_root() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        for kind in 0..3 {
            let (_dir, root, mut resolver) = physical_report_resolver();
            let cancel = AtomicBool::new(false);
            let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &cancel);
            let out = resolver
                .include_capability(
                    IncludeResolveRequest {
                        including_path: &root,
                        byte_range: 0..0,
                        requested_name: "value.inc",
                        legacy_route: None,
                    },
                    &mut budget,
                    &cancel,
                )
                .unwrap();
            let (_, reports) = out.into_parts();
            let before = budget.snapshot();
            let clones = std::cell::Cell::new(0);
            if kind == 0 {
                budget.constrain_retained(before.retained_bytes).unwrap();
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                AdmittedCapabilityReports::copy_with(
                    reports.observations(),
                    reports.warnings(),
                    &mut budget,
                    |o| {
                        clones.set(clones.get() + 1);
                        if kind == 2 {
                            panic!("returned report copy constructor unwind")
                        }
                        if kind == 1 {
                            cancel.store(true, Ordering::Relaxed);
                        }
                        o.clone()
                    },
                    Clone::clone,
                )
            }));
            match kind {
                0 => {
                    assert!(matches!(
                        result.unwrap(),
                        Err(ResolverError::LimitExceeded { .. })
                    ));
                    assert_eq!(clones.get(), 0);
                }
                1 => {
                    assert!(matches!(result.unwrap(), Err(ResolverError::Cancelled)));
                    assert_eq!(clones.get(), 1);
                }
                _ => {
                    assert!(result.is_err());
                    assert_eq!(clones.get(), 1);
                }
            }
            assert_eq!(
                budget.snapshot().retained_bytes,
                before.retained_bytes,
                "partial returned construction cannot refund live persistent/cache/report/source"
            );
            assert!(budget.snapshot().work >= before.work);
            assert!(budget.snapshot().byte_work >= before.byte_work);
            assert!(!reports.observations().is_empty());
            assert!(resolver.callback_reports.is_none());
        }
        let (_dir, root, mut resolver) = physical_report_resolver();
        let mut original = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut foreign = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let out = resolver
            .unit_report_capability(
                UnitResolveRequest {
                    importer_path: &root,
                    requested_name: "Other",
                    legacy_route: None,
                },
                &mut original,
                &NoCancellation,
            )
            .unwrap();
        let (source, reports) = out.into_parts();
        let mut target = foreign.admit_owned(0, 0, 0, Vec::new).unwrap();
        let mut warnings = foreign.admit_owned(0, 0, 0, Vec::new).unwrap();
        let before = foreign.snapshot();
        assert!(matches!(
            reports.consume_into(&mut target, &mut warnings, &mut foreign),
            Err(ResolverError::InvalidRequest(_))
        ));
        assert!(target.is_empty());
        assert!(warnings.is_empty());
        assert_eq!(foreign.snapshot(), before);
        assert!(matches!(source.result(), Resolution::Found(_)));
        assert!(!resolver.report.observations.is_empty());
    }

    #[test]
    fn returned_report_unsupported_capabilities_never_call_legacy_callbacks() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver, ProviderSource};
        struct Legacy;
        impl CompileTimeResolver for Legacy {
            fn include(
                &mut self,
                _: IncludeResolveRequest<'_>,
                _: &mut CompileTimeBudget<'_>,
                _: &dyn CancellationToken,
            ) -> Result<ResolutionOutcome<LoadedSource>, ResolverError> {
                panic!("unbounded legacy include must not run")
            }
            fn unit(
                &mut self,
                _: UnitResolveRequest<'_>,
                _: &mut CompileTimeBudget<'_>,
                _: &dyn CancellationToken,
            ) -> Result<ResolutionOutcome<ProviderSource>, ResolverError> {
                panic!("unbounded legacy unit must not run")
            }
        }
        let mut b = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        assert!(matches!(
            Legacy
                .include_capability(
                    IncludeResolveRequest {
                        including_path: Path::new("Main.pas"),
                        byte_range: 0..0,
                        requested_name: "value.inc",
                        legacy_route: None
                    },
                    &mut b,
                    &NoCancellation
                )
                .unwrap()
                .result(),
            Resolution::Unavailable { .. }
        ));
        assert!(matches!(
            Legacy
                .unit_report_capability(
                    UnitResolveRequest {
                        importer_path: Path::new("Main.pas"),
                        requested_name: "Other",
                        legacy_route: None
                    },
                    &mut b,
                    &NoCancellation
                )
                .unwrap()
                .result(),
            Resolution::Unavailable { .. }
        ));
    }

    #[test]
    fn persistent_report_raw_finish_floor_survives_field_moves_and_unrelated_release() {
        use crate::compile_time::{CompileTimeBudget, CompileTimeLimits, CompileTimeResolver};
        let (_dir, root, mut resolver) = physical_report_resolver();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let observer = budget
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut budget)
            .unwrap();
        let baseline = observer.budget_snapshot().retained_bytes;
        let outcome = CompileTimeResolver::include(
            &mut resolver,
            IncludeResolveRequest {
                including_path: &root,
                byte_range: 0..0,
                requested_name: "value.inc",
                legacy_route: None,
            },
            &mut budget,
            &NoCancellation,
        )
        .unwrap();
        let vector = resolver.report.observations.as_ptr();
        let report = resolver.finish();
        let observations = report.observations;
        assert_eq!(observations.as_ptr(), vector);
        drop(outcome);
        let floor = budget.snapshot();
        assert!(floor.retained_bytes > baseline);
        budget.charge(1, 13, 128).unwrap();
        let temporary = budget.admit_owned(1, 7, 256, || vec![0u8; 256]).unwrap();
        drop(temporary);
        budget.release(usize::MAX);
        assert_eq!(budget.snapshot().retained_bytes, floor.retained_bytes);
        assert!(budget.snapshot().work > floor.work);
        drop(observations);
        assert_eq!(
            budget.snapshot().retained_bytes,
            floor.retained_bytes,
            "raw handoff makes no last-drop promise"
        );
    }

    #[test]
    fn persistent_report_multiple_roots_remain_counted_without_cache_handles() {
        use crate::compile_time::{CompileTimeBudget, CompileTimeLimits, CompileTimeResolver};
        let (_dir, root, mut resolver) = physical_report_resolver();
        let mut a = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut b = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let oa = a
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut a)
            .unwrap();
        let ob = b
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut b)
            .unwrap();
        let ba = oa.budget_snapshot().retained_bytes;
        let bb = ob.budget_snapshot().retained_bytes;
        drop(
            CompileTimeResolver::include(
                &mut resolver,
                IncludeResolveRequest {
                    including_path: &root,
                    byte_range: 0..0,
                    requested_name: "value.inc",
                    legacy_route: None,
                },
                &mut a,
                &NoCancellation,
            )
            .unwrap(),
        );
        drop(
            CompileTimeResolver::unit_capability(
                &mut resolver,
                UnitResolveRequest {
                    importer_path: &root,
                    requested_name: "Other",
                    legacy_route: None,
                },
                &mut b,
                &NoCancellation,
            )
            .unwrap(),
        );
        resolver.compile_time_caches.clear(); // test destruction only; no production eviction
        assert!(oa.budget_snapshot().retained_bytes > ba);
        assert!(ob.budget_snapshot().retained_bytes > bb);
        let usage = resolver.compile_time_usage();
        assert_eq!(
            usage.retained_bytes,
            resolver.compile_time_accounting.retained_bytes
                + oa.budget_snapshot().retained_bytes
                + ob.budget_snapshot().retained_bytes
        );
        assert_eq!(
            usage.work,
            resolver.compile_time_accounting.work
                + oa.budget_snapshot().work
                + ob.budget_snapshot().work
        );
        let report = resolver.finish_admitted(&mut b).unwrap();
        assert!(report.report().observations.iter().any(|o| matches!(o, ResolutionObservation::Payload { path, .. } if path.ends_with("Other.pas"))));
        drop(a);
        drop(b);
        drop(report);
        assert_eq!(oa.budget_snapshot().retained_bytes, ba);
        assert_eq!(ob.budget_snapshot().retained_bytes, bb);
    }

    #[derive(Default)]
    struct MemoryStore {
        sources: HashMap<PathBuf, Vec<u8>>,
    }

    #[test]
    fn persistent_report_refuses_before_format_clone_or_receiving_allocation() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        let (_dir, root, mut resolver) = physical_report_resolver();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        drop(
            CompileTimeResolver::include(
                &mut resolver,
                IncludeResolveRequest {
                    including_path: &root,
                    byte_range: 0..0,
                    requested_name: "value.inc",
                    legacy_route: None,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap(),
        );
        let before = budget.snapshot();
        budget.constrain_retained(before.retained_bytes).unwrap();
        let observations = resolver.report.observations.as_ptr();
        let observation_capacity = resolver.report.observations.capacity();
        let warnings = resolver.report.warnings.as_ptr();
        let warning_capacity = resolver.report.warnings.capacity();
        let calls = std::cell::Cell::new(0);
        struct Formatting<'a>(&'a std::cell::Cell<usize>);
        impl fmt::Display for Formatting<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.set(self.0.get() + 1);
                f.write_str("formatted payload")
            }
        }
        let mut call = resolver.store_call(&mut budget).unwrap();
        assert!(matches!(
            call.warning_args(&NoCancellation, 128, format_args!("{}", Formatting(&calls))),
            Err(ResolverError::LimitExceeded { .. })
        ));
        assert!(matches!(
            call.record_observation_build(128, &NoCancellation, || {
                calls.set(calls.get() + 1);
                ResolutionObservation::Candidate {
                    path: root.clone(),
                    entry: None,
                    stamp: None,
                    present: false,
                }
            }),
            Err(ResolverError::LimitExceeded { .. })
        ));
        assert_eq!(
            calls.get(),
            0,
            "constructors/formatters run only after all peak admissions"
        );
        assert_eq!(call.report.observations.as_ptr(), observations);
        assert_eq!(call.report.observations.capacity(), observation_capacity);
        assert_eq!(call.report.warnings.as_ptr(), warnings);
        assert_eq!(call.report.warnings.capacity(), warning_capacity);
        drop(call);
        assert_eq!(
            budget.snapshot(),
            before,
            "failed preallocation is transactional"
        );
    }

    #[test]
    fn persistent_report_replacement_and_capacity_peak_retire_only_dead_storage() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        let (_dir, root, mut resolver) = physical_report_resolver();
        resolver.limits.max_resolution_warnings = 1;
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        drop(
            CompileTimeResolver::include(
                &mut resolver,
                IncludeResolveRequest {
                    including_path: &root,
                    byte_range: 0..0,
                    requested_name: "value.inc",
                    legacy_route: None,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap(),
        );
        let mut call = resolver.store_call(&mut budget).unwrap();
        call.warning_args(&NoCancellation, 4096, format_args!("first"))
            .unwrap();
        let pointer = call.report.warnings.as_ptr();
        let before = budget.snapshot();
        call.warning_args(&NoCancellation, 64, format_args!("second"))
            .unwrap();
        assert_eq!(call.report.warnings, ["resolution warning limit reached"]);
        assert_eq!(
            call.report.warnings.as_ptr(),
            pointer,
            "replacement retains receiving backing"
        );
        assert_eq!(
            before.retained_bytes - budget.snapshot().retained_bytes,
            4096 - 64
        );
        let spent = budget.snapshot();
        call.warning_args(&NoCancellation, 64, format_args!("third"))
            .unwrap();
        assert_eq!(
            budget.snapshot().retained_bytes,
            spent.retained_bytes,
            "discarded new payload/unused peaks refund exactly"
        );
        assert!(budget.snapshot().work > spent.work);
        call.diagnostic_args(&NoCancellation, 64, format_args!("first diagnostic"))
            .unwrap();
        let before_growth = budget.snapshot();
        let pointer = call.report.incomplete_reasons.as_ptr();
        let peak = 3 * std::mem::size_of::<ReportCredit>()
            + 2 * std::mem::size_of::<String>()
            + 2 * std::mem::size_of::<Option<ReportCredit>>()
            + 64;
        budget
            .constrain_retained(before_growth.retained_bytes + peak - 1)
            .unwrap();
        assert!(matches!(
            call.report_message_in_budget(true, 64, format_args!("second diagnostic"), &mut budget),
            Err(ResolverError::LimitExceeded { .. })
        ));
        assert_eq!(call.report.incomplete_reasons.as_ptr(), pointer);
        assert_eq!(call.report.incomplete_reasons.capacity(), 1);
        assert_eq!(
            budget.snapshot(),
            before_growth,
            "old-plus-new backing peak must be admitted before format or replacement"
        );
        drop(call);
        resolver.compile_time_caches.clear(); // test destruction frees envelope headroom
        let report = resolver.finish_admitted(&mut budget).unwrap();
        drop(report);
        // Historical bytes stay charged even for a discarded warning.
        assert!(budget.snapshot().byte_work >= spent.byte_work);
    }

    #[test]
    fn persistent_report_builder_unwind_refunds_unused_peak_not_previous_report() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        let (_dir, root, mut resolver) = physical_report_resolver();
        let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        drop(
            CompileTimeResolver::include(
                &mut resolver,
                IncludeResolveRequest {
                    including_path: &root,
                    byte_range: 0..0,
                    requested_name: "value.inc",
                    legacy_route: None,
                },
                &mut budget,
                &NoCancellation,
            )
            .unwrap(),
        );
        let pointer = resolver.report.observations.as_ptr();
        let before = budget.snapshot();
        struct PanicFormat;
        impl fmt::Display for PanicFormat {
            fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
                panic!("report constructor unwind")
            }
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            resolver.store_call(&mut budget).unwrap().warning_args(
                &NoCancellation,
                128,
                format_args!("{PanicFormat}"),
            )
        }));
        assert!(result.is_err());
        assert_eq!(resolver.report.observations.as_ptr(), pointer);
        assert!(resolver.report.warnings.is_empty());
        assert_eq!(resolver.report.warnings.capacity(), 0);
        assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
        assert_eq!(budget.snapshot().work, before.work + 1);
        assert_eq!(budget.snapshot().byte_work, before.byte_work + 128);
        assert!(resolver.compile_time_store.is_none());
        assert!(!resolver.report.complete);
    }

    #[test]
    fn persistent_report_legacy_finish_keeps_plain_field_move_contract() {
        let (_dir, _root, resolver) = physical_report_resolver();
        assert_eq!(resolver.report_owners.credits().count(), 0);
        let expected = resolver.report();
        let report = resolver.finish();
        assert_eq!(report, expected);
        let ResolutionReport {
            observations,
            warnings,
            complete,
            incomplete_reasons,
        } = report;
        assert!(complete);
        drop((observations, warnings, incomplete_reasons));
    }

    #[test]
    fn persistent_report_raw_finish_preserves_both_original_root_floors() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        let (_dir, root, mut resolver) = physical_report_resolver();
        let mut a = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let mut b = CompileTimeBudget::new(CompileTimeLimits::default(), &NoCancellation);
        let oa = a
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut a)
            .unwrap();
        let ob = b
            .admit_owned(0, 0, 0, || ())
            .unwrap()
            .into_shared(&mut b)
            .unwrap();
        let ba = oa.budget_snapshot().retained_bytes;
        let bb = ob.budget_snapshot().retained_bytes;
        drop(
            CompileTimeResolver::include(
                &mut resolver,
                IncludeResolveRequest {
                    including_path: &root,
                    byte_range: 0..0,
                    requested_name: "value.inc",
                    legacy_route: None,
                },
                &mut a,
                &NoCancellation,
            )
            .unwrap(),
        );
        drop(
            CompileTimeResolver::unit_capability(
                &mut resolver,
                UnitResolveRequest {
                    importer_path: &root,
                    requested_name: "Other",
                    legacy_route: None,
                },
                &mut b,
                &NoCancellation,
            )
            .unwrap(),
        );
        let pointer = resolver.report.observations.as_ptr();
        let report = resolver.finish();
        assert_eq!(report.observations.as_ptr(), pointer);
        let sa = a.snapshot();
        let sb = b.snapshot();
        assert!(sa.retained_bytes > ba);
        assert!(sb.retained_bytes > bb);
        a.release(usize::MAX);
        b.release(usize::MAX);
        assert_eq!(a.snapshot(), sa);
        assert_eq!(b.snapshot(), sb);
        let ResolutionReport {
            observations,
            warnings,
            incomplete_reasons,
            ..
        } = report;
        drop((observations, warnings, incomplete_reasons));
        assert_eq!(a.snapshot(), sa);
        assert_eq!(b.snapshot(), sb);
        drop(a);
        drop(b);
        assert_eq!(oa.budget_snapshot(), sa);
        assert_eq!(ob.budget_snapshot(), sb);
        // Dropping the last request views ends both ledger/floor lifetimes;
        // there is no global holder, forgotten Arc or permanent allocation.
    }

    #[test]
    fn persistent_report_physical_include_unit_error_cancel_and_unwind() {
        use crate::compile_time::{CompileTimeLimits, CompileTimeResolver};
        #[derive(Clone, Copy)]
        enum Failure {
            Error,
            Cancel,
            Panic,
        }
        struct HookStore {
            inner: FilesystemSourceStore,
            failure: Failure,
            cancelled: Arc<AtomicBool>,
        }
        impl SourceStore for HookStore {
            fn list_directory_admitted(
                &mut self,
                r: DirectoryRequest<'_>,
                b: &mut CompileTimeBudget<'_>,
                c: &dyn CancellationToken,
            ) -> Result<AdmittedOwned<DirectoryListing>, SourceStoreError> {
                self.inner.list_directory_admitted(r, b, c)
            }
            fn overlay_candidates_admitted(
                &self,
                r: &[PathBuf],
                n: &[String],
                b: &mut CompileTimeBudget<'_>,
                c: &dyn CancellationToken,
            ) -> Result<AdmittedOwned<Vec<PathBuf>>, SourceStoreError> {
                self.inner.overlay_candidates_admitted(r, n, b, c)
            }
            fn load_admitted(
                &mut self,
                r: SourceRequest<'_>,
                b: &mut CompileTimeBudget<'_>,
                c: &dyn CancellationToken,
            ) -> Result<LoadedSource, SourceStoreError> {
                if r.path
                    .file_name()
                    .is_some_and(|n| n == "break.inc" || n == "Break.pas")
                {
                    match self.failure {
                        Failure::Error => {
                            return Err(SourceStoreError::Io {
                                path: r.path.into(),
                                message: "injected after candidate observation".into(),
                            });
                        }
                        Failure::Cancel => {
                            self.cancelled.store(true, Ordering::Relaxed);
                            return Err(SourceStoreError::Cancelled);
                        }
                        Failure::Panic => panic!("actual store callback unwind"),
                    }
                }
                self.inner.load_admitted(r, b, c)
            }
            fn list_directory(
                &mut self,
                r: DirectoryRequest<'_>,
                c: &dyn CancellationToken,
            ) -> Result<DirectoryListing, SourceStoreError> {
                self.inner.list_directory(r, c)
            }
            fn overlay_candidates(&self, r: &[PathBuf], n: &[String]) -> Vec<PathBuf> {
                self.inner.overlay_candidates(r, n)
            }
            fn load(
                &mut self,
                r: SourceRequest<'_>,
                c: &dyn CancellationToken,
            ) -> Result<LoadedSource, SourceStoreError> {
                self.inner.load(r, c)
            }
        }
        for unit in [false, true] {
            for failure in [Failure::Error, Failure::Cancel, Failure::Panic] {
                for owned in [false, true] {
                    let (dir, root, original) = physical_report_resolver();
                    fs::write(dir.path().join("break.inc"), "{not returned}").unwrap();
                    fs::write(dir.path().join("Break.pas"), "unit Break;").unwrap();
                    let flag = Arc::new(AtomicBool::new(false));
                    let mut resolver = UnitResolver::new(
                        original.context,
                        vec![dir.path().into()],
                        HookStore {
                            inner: FilesystemSourceStore::new(),
                            failure,
                            cancelled: flag.clone(),
                        },
                        Default::default(),
                    );
                    let mut budget = CompileTimeBudget::new(CompileTimeLimits::default(), &*flag);
                    let observer = budget
                        .admit_owned(0, 0, 0, || ())
                        .unwrap()
                        .into_shared(&mut budget)
                        .unwrap();
                    let baseline = budget.snapshot().retained_bytes;
                    drop(
                        CompileTimeResolver::include(
                            &mut resolver,
                            IncludeResolveRequest {
                                including_path: &root,
                                byte_range: 0..0,
                                requested_name: "value.inc",
                                legacy_route: None,
                            },
                            &mut budget,
                            &*flag,
                        )
                        .unwrap(),
                    );
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if unit && owned {
                            CompileTimeResolver::unit_report_capability(
                                &mut resolver,
                                UnitResolveRequest {
                                    importer_path: &root,
                                    requested_name: "Break",
                                    legacy_route: None,
                                },
                                &mut budget,
                                &*flag,
                            )
                            .map(drop)
                        } else if owned {
                            CompileTimeResolver::include_capability(
                                &mut resolver,
                                IncludeResolveRequest {
                                    including_path: &root,
                                    byte_range: 0..0,
                                    requested_name: "break.inc",
                                    legacy_route: None,
                                },
                                &mut budget,
                                &*flag,
                            )
                            .map(drop)
                        } else if unit {
                            CompileTimeResolver::unit_capability(
                                &mut resolver,
                                UnitResolveRequest {
                                    importer_path: &root,
                                    requested_name: "Break",
                                    legacy_route: None,
                                },
                                &mut budget,
                                &*flag,
                            )
                            .map(drop)
                        } else {
                            CompileTimeResolver::include(
                                &mut resolver,
                                IncludeResolveRequest {
                                    including_path: &root,
                                    byte_range: 0..0,
                                    requested_name: "break.inc",
                                    legacy_route: None,
                                },
                                &mut budget,
                                &*flag,
                            )
                            .map(drop)
                        }
                    }));
                    match failure {
                        Failure::Error => assert!(result.unwrap().is_ok()),
                        Failure::Cancel => {
                            assert_eq!(result.unwrap(), Err(ResolverError::Cancelled))
                        }
                        Failure::Panic => assert!(result.is_err()),
                    }
                    assert!(
                        resolver.compile_time_store.is_none(),
                        "unwind restores callback route"
                    );
                    assert!(!resolver.report.complete);
                    assert!(
                        resolver.callback_reports.is_none(),
                        "returned callback mode/partial reports restore on all exits"
                    );
                    assert!(resolver.report.observations.iter().any(|o| matches!(o, ResolutionObservation::Payload { path, .. } if path.ends_with("value.inc"))));
                    if matches!(failure, Failure::Error) {
                        assert!(
                            !resolver.report.warnings.is_empty()
                                || !resolver.report.incomplete_reasons.is_empty()
                        );
                    }
                    flag.store(false, Ordering::Relaxed);
                    resolver.compile_time_caches.clear(); // only test actual cache destruction
                    let report = resolver.finish_admitted(&mut budget).unwrap();
                    let live = observer.budget_snapshot();
                    assert!(live.retained_bytes > baseline);
                    drop(budget);
                    drop(report);
                    let dead = observer.budget_snapshot();
                    assert_eq!(dead.retained_bytes, baseline);
                    assert_eq!(dead.work, live.work);
                    assert_eq!(dead.byte_work, live.byte_work);
                }
            }
        }
    }

    impl SourceStore for MemoryStore {
        fn list_directory(
            &mut self,
            request: DirectoryRequest<'_>,
            _cancel: &dyn CancellationToken,
        ) -> Result<DirectoryListing, SourceStoreError> {
            let files = self
                .sources
                .keys()
                .filter(|path| path.parent() == Some(request.directory))
                .cloned()
                .collect();
            Ok(DirectoryListing {
                files,
                directories: Vec::new(),
                stamp: None,
                complete: true,
            })
        }

        fn overlay_candidates(&self, _roots: &[PathBuf], _names: &[String]) -> Vec<PathBuf> {
            Vec::new()
        }

        fn load(
            &mut self,
            request: SourceRequest<'_>,
            _cancel: &dyn CancellationToken,
        ) -> Result<LoadedSource, SourceStoreError> {
            let bytes = self.sources.get(request.path).cloned().ok_or_else(|| {
                SourceStoreError::NotFound {
                    path: request.path.to_path_buf(),
                }
            })?;
            Ok(LoadedSource {
                id: SourceId::new(format!("source:{}", request.path.display())),
                path: request.path.to_path_buf(),
                bytes: Arc::<[u8]>::from(bytes),
                decoded_text: None,
                compile_time_backing: None,
                revision: SourceRevision::Overlay {
                    version: 1,
                    content_hash: 0,
                },
            })
        }
    }

    #[test]
    fn source_metadata_is_reparsed_when_a_reload_changes_content() {
        let mut resolver = UnitResolver::new(
            ProjectContext::default(),
            Vec::new(),
            MemoryStore::default(),
            Default::default(),
        );
        let source = |text: &str| LoadedSource {
            id: SourceId::new("source:/workspace/A.pas"),
            path: PathBuf::from("/workspace/A.pas"),
            bytes: Arc::<[u8]>::from(text.as_bytes()),
            decoded_text: None,
            compile_time_backing: None,
            revision: SourceRevision::Overlay {
                version: 1,
                content_hash: 0,
            },
        };

        let before = resolver.source_metadata(&source("unit A; interface implementation end."));
        let same = resolver.source_metadata(&source("unit A; interface implementation end."));
        let after = resolver.source_metadata(&source("unit B; interface implementation end."));

        let mut decoded = source("unit B; interface implementation end.");
        decoded.decoded_text = Some(Arc::from("unit C; interface implementation end."));
        let decoded = resolver.source_metadata(&decoded);

        assert!(Arc::ptr_eq(&before, &same));
        assert_eq!(before.declared_name.as_deref(), Some("A"));
        assert_eq!(after.declared_name.as_deref(), Some("B"));
        assert_eq!(decoded.declared_name.as_deref(), Some("C"));
    }

    #[test]
    fn project_walk_parses_each_source_once() {
        let root = PathBuf::from("/workspace");
        let mut store = MemoryStore::default();
        for (name, text) in [
            (
                "Main.pas",
                "unit Main; interface uses A, B; implementation end.",
            ),
            ("A.pas", "unit A; interface uses B; implementation end."),
            ("B.pas", "unit B; interface implementation end."),
        ] {
            store
                .sources
                .insert(root.join(name), text.as_bytes().to_vec());
        }
        let context = ProjectContext {
            discovery_complete: true,
            search_paths: vec![root.clone()],
            search_path_entries: vec![ProjectPathEntry {
                path: root.clone(),
                provenance: ProjectPathProvenance::LegacyNative,
            }],
            ..ProjectContext::default()
        };
        reset_test_metadata_parses();
        let project = UnitResolver::new(context, vec![root.clone()], store, Default::default())
            .resolve_project(
                UnitResolveRequest {
                    requested_name: "Main",
                    importer_path: &root.join("Main.pas"),
                    legacy_route: None,
                },
                &[],
                &NoCancellation,
            )
            .expect("project graph");

        assert!(project.complete);
        assert_eq!(project.units.len(), 2);
        for name in ["Main.pas", "A.pas", "B.pas"] {
            assert_eq!(
                test_metadata_parses(&root.join(name)),
                1,
                "{name} must be parsed once per resolver"
            );
        }
    }

    #[derive(Default)]
    struct CountingStore {
        listings: usize,
    }

    impl SourceStore for CountingStore {
        fn list_directory(
            &mut self,
            _request: DirectoryRequest<'_>,
            _cancel: &dyn CancellationToken,
        ) -> Result<DirectoryListing, SourceStoreError> {
            self.listings += 1;
            Ok(DirectoryListing {
                files: Vec::new(),
                directories: Vec::new(),
                stamp: None,
                complete: true,
            })
        }

        fn overlay_candidates(&self, _roots: &[PathBuf], _names: &[String]) -> Vec<PathBuf> {
            Vec::new()
        }

        fn load(
            &mut self,
            request: SourceRequest<'_>,
            _cancel: &dyn CancellationToken,
        ) -> Result<LoadedSource, SourceStoreError> {
            Err(SourceStoreError::NotFound {
                path: request.path.to_path_buf(),
            })
        }
    }

    #[test]
    fn spellings_of_one_directory_share_a_listing_and_a_catalogue_on_case_insensitive_volumes() {
        path_identity::with_case_insensitive_volumes(|| {
            let root = PathBuf::from("/workspace");
            let context = ProjectContext {
                discovery_complete: true,
                search_paths: vec![root.clone()],
                search_path_entries: vec![ProjectPathEntry {
                    path: root.clone(),
                    provenance: ProjectPathProvenance::LegacyNative,
                }],
                ..ProjectContext::default()
            };
            let limits = ResolverLimits {
                max_package_catalogues: 1,
                ..Default::default()
            };
            let mut resolver = UnitResolver::new(
                context,
                vec![root.clone()],
                CountingStore::default(),
                limits,
            );

            let lower = resolver
                .catalogue(&root.join("vendor"), &NoCancellation)
                .expect("first catalogue");
            let upper = resolver
                .catalogue(&root.join("VENDOR"), &NoCancellation)
                .expect("second spelling");

            assert!(lower.complete && upper.complete, "{:?}", resolver.report);
            assert!(resolver.report.incomplete_reasons.is_empty());
            assert_eq!(resolver.store.listings, 1, "one listing for both spellings");
        });
    }

    #[cfg(unix)]
    #[derive(Default)]
    struct TreeStore {
        directories: HashMap<PathBuf, (Vec<PathBuf>, Vec<PathBuf>)>,
        listed: Vec<PathBuf>,
    }

    #[cfg(unix)]
    impl SourceStore for TreeStore {
        fn list_directory(
            &mut self,
            request: DirectoryRequest<'_>,
            _cancel: &dyn CancellationToken,
        ) -> Result<DirectoryListing, SourceStoreError> {
            self.listed.push(request.directory.to_path_buf());
            let (files, directories) = self
                .directories
                .get(request.directory)
                .cloned()
                .unwrap_or_default();
            Ok(DirectoryListing {
                files,
                directories,
                stamp: None,
                complete: true,
            })
        }

        fn overlay_candidates(&self, _roots: &[PathBuf], _names: &[String]) -> Vec<PathBuf> {
            Vec::new()
        }

        fn load(
            &mut self,
            request: SourceRequest<'_>,
            _cancel: &dyn CancellationToken,
        ) -> Result<LoadedSource, SourceStoreError> {
            Err(SourceStoreError::NotFound {
                path: request.path.to_path_buf(),
            })
        }
    }

    // `\` is an ordinary name byte off Windows, and names need not be UTF-8;
    // directories whose names only print alike are still distinct.
    #[cfg(unix)]
    #[test]
    fn directories_that_only_print_alike_keep_their_own_listings_and_catalogues() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let root = PathBuf::from("/workspace");
        let package = root.join("pkg");
        let backslash = package.join("a\\b");
        let nested = package.join("a").join("b");
        let latin1_y = package.join(OsStr::from_bytes(b"lib\xff"));
        let latin1_p = package.join(OsStr::from_bytes(b"lib\xfe"));
        let mut store = TreeStore::default();
        store.directories.insert(
            package.clone(),
            (
                Vec::new(),
                vec![
                    backslash.clone(),
                    package.join("a"),
                    latin1_y.clone(),
                    latin1_p.clone(),
                ],
            ),
        );
        store
            .directories
            .insert(package.join("a"), (Vec::new(), vec![nested.clone()]));
        for (directory, unit) in [
            (&backslash, "Backslash.pas"),
            (&nested, "Nested.pas"),
            (&latin1_y, "LatinY.pas"),
            (&latin1_p, "LatinP.pas"),
        ] {
            store
                .directories
                .insert(directory.clone(), (vec![directory.join(unit)], Vec::new()));
        }
        let context = ProjectContext {
            discovery_complete: true,
            search_paths: vec![root.clone()],
            search_path_entries: vec![ProjectPathEntry {
                path: root.clone(),
                provenance: ProjectPathProvenance::LegacyNative,
            }],
            ..ProjectContext::default()
        };
        let mut resolver =
            UnitResolver::new(context, vec![root.clone()], store, Default::default());

        let catalogue = resolver
            .catalogue(&package, &NoCancellation)
            .expect("package catalogue");
        let mut units = catalogue.entries.keys().cloned().collect::<Vec<_>>();
        units.sort();
        assert_eq!(
            units,
            ["backslash.pas", "latinp.pas", "latiny.pas", "nested.pas"],
            "{:?}",
            resolver.report
        );

        let entry = ProjectPathEntry {
            path: root.clone(),
            provenance: ProjectPathProvenance::LegacyNative,
        };
        let listed_before = resolver.store.listed.len();
        for (first, second) in [(&backslash, &nested), (&latin1_y, &latin1_p)] {
            for directory in [first, second] {
                let entry = ProjectPathEntry {
                    path: directory.clone(),
                    ..entry.clone()
                };
                resolver
                    .list_directory(directory, &entry, None, &NoCancellation, false)
                    .expect("listing");
            }
        }
        assert_eq!(
            resolver.store.listed.len() - listed_before,
            4,
            "each directory needs its own listing"
        );

        let limits = ResolverLimits {
            max_package_catalogues: 2,
            ..Default::default()
        };
        let mut resolver = UnitResolver::new(
            resolver.context.clone(),
            vec![root.clone()],
            TreeStore {
                directories: resolver.store.directories.clone(),
                listed: Vec::new(),
            },
            limits,
        );
        let first = resolver
            .catalogue(&latin1_y, &NoCancellation)
            .expect("first catalogue");
        let second = resolver
            .catalogue(&latin1_p, &NoCancellation)
            .expect("second catalogue");
        assert!(first.entries.contains_key("latiny.pas"));
        assert!(second.entries.contains_key("latinp.pas"));
    }
}

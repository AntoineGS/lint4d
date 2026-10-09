//! Request-scoped Pascal source resolution.
//!
//! The resolver owns lookup policy and bounded observations, while callers own
//! project discovery state and provide a [`SourceStore`] for disk or overlay
//! bytes.  No result is considered precise unless the candidate was loaded and
//! its declaration matched the requested name.

use crate::compile_time::{AdmittedOwned, CompileTimeBudget};
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
}

#[derive(Default)]
struct StoreCache {
    loaded: HashMap<SourceId, LoadedSource>,
    directories: HashMap<DirectoryCacheKey, crate::compile_time::AdmittedOwned<DirectoryListing>>,
    loaded_capacity_credit: usize,
    directory_capacity_credit: usize,
    recorded_work: usize,
    recorded_byte_work: usize,
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
        self.loaded.retain(|id, _| !self.include_seen.contains(id));
        self.include_seen.clear();
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
                let reason = error.to_string();
                Resolution::Incomplete {
                    reason,
                    candidates: Vec::new(),
                }
            }
        };
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
        let names = self
            .context
            .unit_namespaces
            .iter()
            .map(|n| n.len() + std::mem::size_of::<String>())
            .sum::<usize>();
        let paths = self
            .context
            .search_path_entries
            .iter()
            .chain(&self.context.browsing_path_entries)
            .map(|e| e.path.as_os_str().len() + std::mem::size_of::<ProjectPathEntry>())
            .sum::<usize>();
        let explicit = self
            .context
            .explicit_unit_entries
            .values()
            .flatten()
            .map(|e| {
                2 * e.path.as_os_str().len() + std::mem::size_of::<(PathBuf, ProjectPathEntry)>()
            })
            .sum::<usize>();
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
            return Err(ResolverError::InvalidRequest(
                "unit name is empty".to_string(),
            ));
        }
        if !self.context.can_resolve_units() {
            self.diagnostic_args(cancel, 30, format_args!("project metadata is incomplete"))?;
            return Ok(Resolution::Incomplete {
                reason: "project metadata is incomplete".to_string(),
                candidates: Vec::new(),
            });
        }
        let lookup = aliased_name(&self.context, requested);
        if let Some(issue) = self.context.missing_explicit_unit(&lookup) {
            let reason = issue.path.as_ref().map_or_else(
                || format!("explicit unit {lookup} is missing"),
                |path| format!("explicit unit {lookup} is missing: {}", path.display()),
            );
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
                paths: entries
                    .iter()
                    .map(|entry| (entry.path.clone(), entry.clone()))
                    .collect(),
                legacy_route: request.legacy_route.cloned(),
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
            let paths = paths
                .iter()
                .filter_map(|path| {
                    self.context
                        .path_entry_for(path)
                        .or_else(|| self.legacy_entry_for_path(path, request.legacy_route))
                        .map(|entry| (path.clone(), entry))
                })
                .collect();
            let group = CandidateGroup {
                paths,
                legacy_route: request.legacy_route.cloned(),
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
                reason: format!("no authorized unit candidate for {requested}"),
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
        let bytes = directory.as_os_str().len().saturating_mul(8)
            + unit_name.len().saturating_mul(8)
            + namespaces
                .iter()
                .map(|n| 8 * (n.len() + std::mem::size_of::<String>()))
                .sum::<usize>();
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
                reason: format!(
                    "directory scan under {} was incomplete",
                    directory.display()
                ),
                candidates: Vec::new(),
            }));
        }
        let tiers = filename_tiers(unit_name, namespaces);
        for names in tiers {
            let paths = self.paths_for_names(&directory, &entry, &names, &listing, cancel)?;
            let group = CandidateGroup {
                paths,
                legacy_route: legacy_route.cloned(),
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
                        return Ok(Some(Resolution::Ambiguous {
                            candidates: paths
                                .into_iter()
                                .map(|path| ResolutionCandidate {
                                    path,
                                    declared_name: None,
                                })
                                .collect(),
                        }));
                    }
                    CaseInsensitiveLookup::Incomplete => {
                        return Ok(Some(Resolution::Incomplete {
                            reason: "case-insensitive path lookup was incomplete".to_string(),
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
                            reason: format!(
                                "directory scan under {} was incomplete",
                                nested.display()
                            ),
                            candidates: Vec::new(),
                        }));
                    }
                    for names in filename_tiers(parts.last().copied().unwrap_or_default(), &[]) {
                        let group = CandidateGroup {
                            paths: self.paths_for_names(
                                &nested,
                                &nested_entry,
                                &names,
                                &nested_listing,
                                cancel,
                            )?,
                            legacy_route: legacy_route.cloned(),
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
        self.check_cancel(cancel)?;
        let copies = group
            .paths
            .iter()
            .map(|(p, e)| {
                8 * (p.as_os_str().len()
                    + e.path.as_os_str().len()
                    + std::mem::size_of::<ResolutionCandidate>())
            })
            .sum::<usize>();
        self.admit_lookup_storage(1, copies, copies, cancel)?;
        let mut valid = Vec::new();
        let mut candidates = Vec::new();
        for (path, entry) in dedup_paths(group.paths.clone()) {
            self.check_cancel(cancel)?;
            match self.load_candidate(
                &path,
                &entry,
                group.legacy_route.as_ref(),
                SourceKind::Unit,
                cancel,
            ) {
                Ok(source) => {
                    let declared_name = if self.compile_time_store.is_some() {
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
                    let candidate = ResolutionCandidate {
                        path: source.path.clone(),
                        declared_name: declared_name.clone(),
                    };
                    if declared_name_matches(
                        declared_name.as_deref(),
                        requested,
                        lookup,
                        &self.context,
                    ) {
                        valid.push((source, candidate));
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
                        reason: "unit candidate is unauthorized".to_string(),
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
                    return Ok(Some(Resolution::Incomplete { reason, candidates }));
                }
                Err(error) => {
                    self.diagnostic_args(
                        cancel,
                        source_error_report_bound(&error),
                        format_args!("unit candidate could not be loaded: {error:?}"),
                    )?;
                    let reason = format!("unit candidate could not be loaded: {error:?}");
                    return Ok(Some(Resolution::Incomplete { reason, candidates }));
                }
            }
        }
        match valid.len() {
            0 => Ok(None),
            1 => {
                let (source, candidate) = valid.pop().expect("one valid candidate");
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
                let candidates = valid.into_iter().map(|(_, candidate)| candidate).collect();
                self.warning_args(
                    cancel,
                    64 + requested.len(),
                    format_args!("ambiguous unit {requested}: multiple valid candidates"),
                )?;
                Ok(Some(Resolution::Ambiguous { candidates }))
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
    ) -> Result<Vec<(PathBuf, ProjectPathEntry)>, ResolverError> {
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
        Ok(paths)
    }

    fn list_directory(
        &mut self,
        directory: &Path,
        entry: &ProjectPathEntry,
        _legacy_route: Option<&LegacyRoute>,
        cancel: &dyn CancellationToken,
        missing_is_incomplete: bool,
    ) -> Result<DirectoryListing, ResolverError> {
        self.check_cancel(cancel)?;
        let mut key_bytes =
            4 * directory.as_os_str().len() + std::mem::size_of::<DirectoryCacheKey>();
        self.context
            .read_policy
            .visit_owned_clone_payload(&mut |n| {
                key_bytes += n;
                Ok(())
            })
            .expect("infallible metadata sizing");
        self.admit_lookup_storage(1, key_bytes, key_bytes, cancel)?;
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
            return Ok(listing);
        }
        let request = DirectoryRequest {
            directory: &key,
            entry,
            read_policy: &self.context.read_policy,
        };
        let result = if let Some(mut budget) = self.store_budget(cancel) {
            match self
                .store
                .list_directory_admitted(request, &mut budget, cancel)
            {
                Ok(mut owner) => {
                    let bytes = owner
                        .files
                        .iter()
                        .chain(&owner.directories)
                        .map(|p| p.as_os_str().len() + std::mem::size_of::<PathBuf>())
                        .sum::<usize>();
                    let listing = owner
                        .grow(
                            &mut budget,
                            1,
                            bytes.saturating_mul(2),
                            bytes.saturating_mul(2)
                                + std::mem::size_of::<
                                    crate::compile_time::AdmittedOwned<DirectoryListing>,
                                >(),
                            |v| v.clone(),
                        )
                        .map_err(|r| root_store_error(store_admission_error(r, &budget)))?;
                    let root = self
                        .compile_time_store
                        .as_ref()
                        .expect("admitted store call")
                        .clone();
                    let mut cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
                    let old = cache.directory_capacity_credit;
                    let growth = cache.directories.len() == cache.directories.capacity();
                    let new = if growth {
                        ((cache.directories.len() + 1).next_power_of_two() * 2).max(4)
                            * (std::mem::size_of::<(
                                DirectoryCacheKey,
                                crate::compile_time::AdmittedOwned<DirectoryListing>,
                            )>() + 1)
                            + 16
                    } else {
                        0
                    };
                    cache
                        .grow(&mut budget, 1, key_bytes, key_bytes + new, |c| {
                            if growth {
                                c.directories.reserve(1);
                                c.directory_capacity_credit = new;
                            }
                            c.directories.insert(cache_key.clone(), owner);
                        })
                        .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
                    if growth {
                        cache
                            .retire_storage(old)
                            .map_err(|r| crate::compile_time::resolver_error(r, &budget))?;
                    }
                    Ok(listing)
                }
                Err(error) => Err(error),
            }
        } else {
            self.store.list_directory(request, cancel)
        };
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
        Ok(listing)
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
        self.check_cancel(cancel)
            .map_err(|_| SourceStoreError::Cancelled)?;
        // Canonical path and local entry scratch only. Persistent candidate /
        // revision observation payload and slots now have report receipts.
        let mut provenance = 0usize;
        entry
            .visit_recovery_payload(&mut |n| {
                provenance = provenance.saturating_add(n);
                Ok(())
            })
            .expect("infallible local entry bound");
        let metadata = path
            .as_os_str()
            .len()
            .saturating_mul(4)
            .saturating_add(provenance.saturating_sub(entry.path.as_os_str().len()));
        self.admit_lookup_storage(1, metadata, metadata, cancel)
            .map_err(|error| match error {
                ResolverError::LimitExceeded {
                    limit,
                    observed,
                    maximum,
                } => SourceStoreError::RootLimit {
                    limit,
                    observed,
                    maximum,
                },
                _ => SourceStoreError::Cancelled,
            })?;
        let path = canonical_path(path);
        let entry = ProjectPathEntry {
            path: path.clone(),
            provenance: entry.provenance.clone(),
        };
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
        if let Some(mut budget) = self.store_budget(cancel) {
            let cached = loaded.admitted_copy(&mut budget)?;
            let root = self
                .compile_time_store
                .as_ref()
                .expect("store call")
                .clone();
            let mut cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
            let old = cache.loaded_capacity_credit;
            let growth = cache.loaded.len() == cache.loaded.capacity();
            let new = if growth {
                ((cache.loaded.len() + 1).next_power_of_two() * 2).max(4)
                    * (std::mem::size_of::<(SourceId, LoadedSource)>() + 1)
                    + 16
            } else {
                0
            };
            cache
                .grow(
                    &mut budget,
                    1,
                    loaded.id.as_str().len(),
                    new + loaded.id.as_str().len(),
                    |c| {
                        if growth {
                            c.loaded.reserve(1);
                            c.loaded_capacity_credit = new;
                        }
                        c.loaded.insert(loaded.id.clone(), cached);
                    },
                )
                .map_err(|r| store_admission_error(r, &budget))?;
            if growth {
                cache
                    .retire_storage(old)
                    .map_err(|r| store_admission_error(r, &budget))?;
            }
        } else {
            self.loaded.insert(loaded.id.clone(), loaded.clone());
        }
        let mut observation_bytes = loaded.id.as_str().len() + loaded.path.as_os_str().len();
        if let SourceRevision::Disk {
            read_policy,
            path_entry,
            ..
        } = &loaded.revision
        {
            read_policy
                .visit_owned_clone_payload(&mut |n| {
                    observation_bytes += n;
                    Ok(())
                })
                .expect("infallible revision bound");
            path_entry
                .visit_recovery_payload(&mut |n| {
                    observation_bytes += n;
                    Ok(())
                })
                .expect("infallible path entry bound");
        }
        self.record_observation_build(observation_bytes, cancel, || {
            ResolutionObservation::Payload {
                source_id: loaded.id.clone(),
                path: loaded.path.clone(),
                revision: loaded.revision.clone(),
            }
        })
        .map_err(resolver_report_store_error)?;
        Ok(loaded)
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

    fn resolve_include_inner(
        &mut self,
        request: IncludeResolveRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<Resolution<ResolvedSource>, ResolverError> {
        self.check_cancel(cancel)?;
        let paths = self
            .context
            .include_paths
            .iter()
            .chain(&self.context.search_paths)
            .map(|p| p.as_os_str().len() + std::mem::size_of::<ProjectPathEntry>())
            .sum::<usize>();
        let reserve = paths.saturating_mul(4)
            + request.including_path.as_os_str().len().saturating_mul(8)
            + request.requested_name.len().saturating_mul(8);
        self.admit_lookup_storage(1, reserve, reserve, cancel)?;
        if request.requested_name.trim().is_empty() {
            return Ok(Resolution::Incomplete {
                reason: "include directive has no file name".to_string(),
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
        let mut candidates = Vec::new();
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
                Ok(source) => candidates.push(source),
                Err(SourceStoreError::NotFound { .. }) => {
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
                                Ok(source) => candidates.push(source),
                                Err(SourceStoreError::NotFound { .. }) => {}
                                Err(SourceStoreError::Cancelled) => {
                                    return Err(ResolverError::Cancelled);
                                }
                                Err(error @ SourceStoreError::RootLimit { .. }) => {
                                    return Err(root_store_error(error));
                                }
                                Err(error) => {
                                    saw_incomplete = true;
                                    self.warning_args(
                                        cancel,
                                        source_error_report_bound(&error),
                                        format_args!(
                                            "include candidate could not be loaded: {error:?}"
                                        ),
                                    )?;
                                    break;
                                }
                            }
                        }
                        CaseInsensitiveLookup::Missing => {}
                        CaseInsensitiveLookup::Ambiguous(paths) => {
                            saw_incomplete = true;
                            let bound = 128
                                + request.requested_name.len()
                                + paths
                                    .iter()
                                    .map(|p| 4 * p.as_os_str().len() + 2)
                                    .sum::<usize>();
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
                Err(SourceStoreError::Cancelled) => return Err(ResolverError::Cancelled),
                Err(error @ SourceStoreError::RootLimit { .. }) => {
                    return Err(root_store_error(error));
                }
                Err(error) => {
                    saw_incomplete = true;
                    self.warning_args(
                        cancel,
                        source_error_report_bound(&error),
                        format_args!("include candidate could not be loaded: {error:?}"),
                    )?;
                    break;
                }
            }
            if !candidates.is_empty() {
                break;
            }
        }
        candidates.sort_by_key(|left| path_key(&left.path));
        candidates.dedup_by(|left, right| left.id == right.id);
        match candidates.len() {
            1 => Ok(Resolution::Found(
                candidates.pop().expect("one include candidate"),
            )),
            0 if saw_incomplete => {
                self.diagnostic_args(
                    cancel,
                    64 + request.requested_name.len(),
                    format_args!(
                        "include {} could not be resolved completely",
                        request.requested_name
                    ),
                )?;
                Ok(Resolution::Incomplete {
                    reason: format!("include {} lookup was incomplete", request.requested_name),
                    candidates: Vec::new(),
                })
            }
            0 => Ok(Resolution::Unavailable {
                reason: format!("include {} was not found", request.requested_name),
            }),
            _ => Ok(Resolution::Ambiguous {
                candidates: candidates
                    .into_iter()
                    .map(|source| ResolutionCandidate {
                        path: source.path,
                        declared_name: None,
                    })
                    .collect(),
            }),
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
    ) -> Result<LoadedSource, SourceStoreError> {
        let source_id = source_id_for_path(path);
        let existing = if let Some(mut budget) = self.store_budget(cancel) {
            let root = self
                .compile_time_store
                .as_ref()
                .expect("store call")
                .clone();
            let cache = root.view.lock().unwrap_or_else(|e| e.into_inner());
            cache
                .loaded
                .values()
                .find(|source| {
                    path_equivalent(&source.path, path)
                        && self.can_reuse_loaded_source(path, entry, legacy_route)
                })
                .map(|source| source.admitted_copy(&mut budget))
                .transpose()?
        } else {
            self.loaded
                .values()
                .find(|source| {
                    path_equivalent(&source.path, path)
                        && self.can_reuse_loaded_source(path, entry, legacy_route)
                })
                .cloned()
        };
        let known_id = existing
            .as_ref()
            .map_or_else(|| source_id.clone(), |source| source.id.clone());
        let is_new = !self.include_seen.contains(&known_id);
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
                return Ok(CaseInsensitiveLookup::Ambiguous(matches));
            } else {
                return Ok(CaseInsensitiveLookup::Missing);
            }
        }
        Ok(CaseInsensitiveLookup::Found(current))
    }

    fn filename_catalogue_candidates(
        &mut self,
        unit_name: &str,
        cancel: &dyn CancellationToken,
    ) -> Result<Vec<CandidateGroup>, ResolverError> {
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
                paths,
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
            for child in listing.directories {
                let Some(child_entry) = self.context.path_entry_for(&child) else {
                    continue;
                };
                if self.context.read_policy.allows_location(&child_entry) {
                    queue.push_back(child);
                }
            }
            for file in listing.files {
                let Some(file_entry) = self.context.path_entry_for(&file) else {
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
                    entries.entry(key).or_insert_with(Vec::new).push(file);
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
            self.admit_lookup_storage(1, reason.len(), reason.len(), cancel)?;
            return Ok(Resolution::Incomplete {
                reason: reason.into(),
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
    fn unit_capability(
        &mut self,
        request: UnitResolveRequest<'_>,
        budget: &mut crate::compile_time::CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<ResolutionOutcome<crate::compile_time::AdmittedProviderSource>, ResolverError> {
        let outcome =
            crate::compile_time::CompileTimeResolver::unit(self, request, budget, cancel)?;
        let result = match outcome.result {
            Resolution::Found(provider) => {
                let backing = crate::compile_time::ProviderSource::admit_cached_backing(
                    &provider.unit.source,
                    budget,
                )?;
                Resolution::Found(provider.admit_copy(&backing, budget)?)
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
    let mut total = std::mem::size_of::<pascal_project::ConditionalContext>();
    for candidate in [
        &context.conditional_context,
        &context.library_conditional_context,
    ] {
        candidate
            .visit_recovery_payload(&mut |n| {
                total = total
                    .checked_add(n)
                    .ok_or_else(|| "context admission overflow".to_string())?;
                Ok(())
            })
            .map_err(ResolverError::InvalidRequest)?;
        total = total
            .checked_add(
                (candidate.defines.len() + candidate.options.len() + candidate.constants.len())
                    * (std::mem::size_of::<(String, crate::conditional::ConstantValue)>()
                        + 4 * std::mem::size_of::<usize>()),
            )
            .ok_or_else(|| ResolverError::InvalidRequest("context admission overflow".into()))?;
    }
    for define in &context.defines {
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

struct CandidateGroup {
    paths: Vec<(PathBuf, ProjectPathEntry)>,
    legacy_route: Option<LegacyRoute>,
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
    Found(PathBuf),
    Ambiguous(Vec<PathBuf>),
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
    let mut qualifiers = Vec::new();
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

fn dedup_paths(paths: Vec<(PathBuf, ProjectPathEntry)>) -> Vec<(PathBuf, ProjectPathEntry)> {
    let mut result: Vec<(PathBuf, ProjectPathEntry)> = Vec::new();
    for (path, entry) in paths {
        if !result
            .iter()
            .any(|(existing, _)| path_equivalent(existing, &path))
        {
            result.push((path, entry));
        }
    }
    result.sort_by_key(|left| path_key(&left.0));
    result
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
                    if unit {
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
                    Failure::Cancel => assert_eq!(result.unwrap(), Err(ResolverError::Cancelled)),
                    Failure::Panic => assert!(result.is_err()),
                }
                assert!(
                    resolver.compile_time_store.is_none(),
                    "unwind restores callback route"
                );
                assert!(!resolver.report.complete);
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

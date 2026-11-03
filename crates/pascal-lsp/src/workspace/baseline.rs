//! Baseline snapshots (spec §6.3 amendments 2026-11-03, D31, D32, D39,
//! D40). A baseline view seals a project context's complete disk-only
//! membership once every member has a proven record, holds the newest
//! snapshot it sealed or validated, and a later process starts from that
//! snapshot: it finds each member's record through it instead of searching
//! evidence candidates, and still validates the content (D38).

use super::authorize::authorize_record;
use super::capture::ReadCapture;
use super::eligible::{CapturedEvidence, Ineligible, SealFailure, capture_evidence, seal_baseline};
use super::evidence::EvidenceGap;
use super::persist::{PersistError, Reuse, ReuseMiss, ReuseVia, admitted, decoded};
use super::validate::{CurrentInputs, EvidenceCheck, Validator, validate_evidence};
use super::{ContextKey, PathStamp, Workspace};
use crate::catalogue::CatalogueKey;
use crate::persistent_cache::CacheJob;
use crate::project_cache::ReservationKind;
use lsp_types::Url;
use pascal_cache::{
    Bypass, ContextKey as CacheContextKey, Digest, DiscoveryInputs, DiscoveryScope,
    EvidencePurpose, LocalArtifactKey, Lookup, Miss, RecordListDigest, ScannedDirectory,
    SnapshotHeader, SnapshotId, SourceContent, Store,
};
use pascal_core::facts::FactBudget;
use pascal_core::resolver::{
    DirectoryListing, DirectoryRequest, FilesystemSourceStore, ResolutionObservation,
    ResolverLimits, SourceStore, SourceStoreError,
};
use pascal_project::{ProjectPathEntry, ProjectWorkBudget};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// A context is sealed at most once a minute (§6.3 amendment, D31).
const SEAL_INTERVAL: Duration = Duration::from_secs(60);

/// The snapshot a context's index starts from or last sealed (D40).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BaselineHint {
    header: SnapshotHeader,
    /// `RecordListDigest` of the snapshot's ordered record IDs.
    records: Digest,
    /// This process's lease holds it. A refused hold (`WalPressure`) leaves
    /// a usable hint that maintenance may retire: it is only a hint.
    held: bool,
    /// This process sealed it, rather than adopting it at startup.
    sealed: bool,
}

impl BaselineHint {
    pub(crate) fn header(&self) -> &SnapshotHeader {
        &self.header
    }

    #[cfg_attr(not(test), allow(dead_code))] // pascal/indexStatus (l4d-zf1b.8)
    pub(crate) fn records(&self) -> Digest {
        self.records
    }

    pub(crate) fn held(&self) -> bool {
        self.held
    }

    #[cfg_attr(not(test), allow(dead_code))] // pascal/indexStatus (l4d-zf1b.8)
    pub(crate) fn sealed(&self) -> bool {
        self.sealed
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SealOutcome {
    /// Not a complete project index, sealed within the last minute, or no
    /// store (or no room to build the record list).
    NotDue,
    /// The records and discovery evidence are the held snapshot's.
    Unchanged,
    Sealed,
    /// `check` stopped the build, which was retired; retried after the next
    /// complete batch.
    Interrupted,
    Ineligible(Ineligible),
    /// The discovery evidence could not be captured; a changed directory
    /// made the membership dirty.
    Gap(BaselineGap),
    /// A store bypass retired the build.
    Bypassed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BaselineGap {
    /// No project file (D39), or an unknown context.
    NotAProject,
    /// The directory's stamp no longer proves its catalogue's listing: the
    /// membership is marked dirty and recomputed by the next batch.
    Changed(PathBuf),
    Capture(EvidenceGap),
    /// The catalogue the membership was read from is gone or was replaced.
    Unlisted(PathBuf),
}

/// What looking a member up through the held snapshot gave.
pub(super) enum HintReuse {
    /// Decided: a hit, or a retryable miss (budget, memory).
    Done(Reuse),
    /// The snapshot's record has no artifact (a refusal or a limit): retry
    /// it after every other member (H4).
    Retrying,
    /// The snapshot was retired or deleted: drop the hint.
    Gone,
    /// The hint cannot prove this member: search evidence candidates.
    Fallback,
}

/// Whether a startup adoption decided anything.
enum Adoption {
    Adopted,
    None,
    /// A retryable miss (budget, cancellation, a bypass): try again later.
    Retry,
}

#[cfg(test)]
thread_local! {
    static HINT_REUSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Members reused through a held snapshot on this thread.
#[cfg(test)]
pub(crate) fn hint_reuses_for_test() -> usize {
    HINT_REUSES.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(crate) fn reset_hint_reuses_for_test() {
    HINT_REUSES.with(|count| count.set(0));
}

/// The scope of a scan of `directories`, in order, none recursive.
fn discovery_scope(directories: &[Directory]) -> DiscoveryScope {
    let scanned = directories
        .iter()
        .map(|directory| ScannedDirectory {
            path: &directory.path,
            recursive: false,
        })
        .collect::<Vec<_>>();
    DiscoveryScope::new(&DiscoveryInputs {
        directories: &scanned,
        exclusions: &[],
    })
}

/// The digest of a sealed snapshot's record IDs as its verified stream
/// gives them; `None` when the snapshot is no longer readable.
fn snapshot_records(store: &Store, header: &SnapshotHeader) -> Result<Option<Digest>, Bypass> {
    let mut reader = store.read_snapshot(header);
    let mut records = RecordListDigest::new();
    loop {
        match reader.next_batch()? {
            Lookup::Hit(Some(batch)) => {
                for member in batch.members() {
                    records.push(&member.record_id);
                }
            }
            Lookup::Hit(None) => break,
            Lookup::Miss(_) => return Ok(None),
        }
    }
    Ok(reader.is_verified().then(|| records.finish()))
}

fn record_list(records: &[Digest]) -> Digest {
    let mut list = RecordListDigest::new();
    for record in records {
        list.push(record);
    }
    list.finish()
}

fn release(job: &CacheJob, id: SnapshotId) {
    if let Err(bypass) = job.store().release_snapshot(id) {
        job.observe(&bypass);
    }
}

/// One membership directory: a catalogue's listed directory with the stamp
/// the catalogue recorded before listing it.
struct Directory {
    path: PathBuf,
    recorded: Option<PathStamp>,
    scope: CatalogueKey,
    generation: u64,
}

impl Directory {
    /// Whether `current` proves the directory unchanged since its catalogue
    /// listed it; a missing directory stays missing.
    fn proves(&self, current: Option<&PathStamp>) -> bool {
        match (&self.recorded, current) {
            (Some(recorded), Some(current)) => recorded.proves_unchanged(current),
            (None, None) => true,
            _ => false,
        }
    }
}

/// The request's cancellation over a work budget (the validator polls the
/// budget's `check_cancelled`).
struct RequestCancel<'a> {
    work: &'a dyn ProjectWorkBudget,
    cancel: &'a AtomicBool,
}

impl ProjectWorkBudget for RequestCancel<'_> {
    fn check_cancelled(&self) -> Result<(), String> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(super::rename::CANCELLATION_MESSAGE.to_owned());
        }
        self.work.check_cancelled()
    }

    fn charge_path_visits(&self, amount: usize) -> Result<(), String> {
        self.work.charge_path_visits(amount)
    }

    fn charge_path_bytes(&self, amount: usize) -> Result<(), String> {
        self.work.charge_path_bytes(amount)
    }

    fn ensure_file_read_fits(&self, max_bytes: usize) -> Result<(), String> {
        self.work.ensure_file_read_fits(max_bytes)
    }

    fn charge_file_bytes(&self, amount: usize) -> Result<(), String> {
        self.work.charge_file_bytes(amount)
    }

    fn is_transient_error(&self, error: &str) -> bool {
        self.work.is_transient_error(error)
    }
}

impl Workspace {
    /// The directories `key`'s membership was read from, in scan order:
    /// the main source's, the explicit units', then the search paths, as
    /// `index_membership` records its catalogues.
    fn membership_directories(&self, key: &ContextKey) -> Result<Vec<Directory>, BaselineGap> {
        let state = self.contexts.get(key).ok_or(BaselineGap::NotAProject)?;
        let project = state
            .context
            .project_file()
            .ok_or(BaselineGap::NotAProject)?;
        let scopes = self
            .project_cache
            .read_index(key, |_, membership| membership.scopes.clone())
            .ok_or_else(|| BaselineGap::Unlisted(project.to_path_buf()))?;
        let mut directories = Vec::new();
        for (scope, generation) in scopes {
            let shared = self
                .project_cache
                .catalogue(&scope)
                .filter(|shared| generation != 0 && shared.generation == generation)
                .ok_or_else(|| BaselineGap::Unlisted(scope.root.clone()))?;
            for (path, recorded) in shared.catalogue.directories() {
                directories.push(Directory {
                    path: path.clone(),
                    recorded: recorded.clone(),
                    scope: scope.clone(),
                    generation,
                });
            }
        }
        Ok(directories)
    }

    /// One listing observation per membership directory, in scan order,
    /// captured in this baseline view; each directory's stamp must still
    /// prove the catalogue's (`Catalogue::directories`) unchanged, before
    /// listing and as the listing itself stamped it. A full listing changes
    /// with unrelated churn too, which only costs the hint. `check` runs
    /// before each directory (gate, cancellation).
    pub(crate) fn discovery_evidence(
        &self,
        key: &ContextKey,
        check: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(DiscoveryScope, CapturedEvidence), BaselineGap> {
        let directories = self.membership_directories(key)?;
        let state = &self.contexts[key];
        let context = &state.context;
        let policy = context.read_policy();
        // The validator's listing (`Validator::new`), so both agree.
        let mut disk = FilesystemSourceStore::with_limits(
            ResolverLimits::default().max_directory_entries,
            usize::MAX,
        );
        let capture = ReadCapture::default();
        let cancel = AtomicBool::new(false);
        let mut observations = Vec::with_capacity(directories.len());
        let changed = |directory: &Directory| {
            self.project_cache
                .drop_catalogue(&directory.scope, directory.generation);
            self.project_cache.mark_index_membership_dirty(key);
            BaselineGap::Changed(directory.path.clone())
        };
        for directory in &directories {
            if check().is_err() {
                return Err(BaselineGap::Capture(EvidenceGap::Cancelled));
            }
            if !directory.proves(super::path_stamp(&directory.path).as_ref()) {
                return Err(changed(directory));
            }
            let entry = context
                .path_entry_for(&directory.path)
                .unwrap_or_else(|| ProjectPathEntry::configured(directory.path.clone()));
            let listing = match disk.list_directory(
                DirectoryRequest {
                    directory: &directory.path,
                    entry: &entry,
                    read_policy: policy,
                },
                &cancel,
            ) {
                Ok(listing) => listing,
                // As the session saw it: a missing directory lists nothing.
                Err(SourceStoreError::NotFound { .. }) => DirectoryListing {
                    files: Vec::new(),
                    directories: Vec::new(),
                    stamp: None,
                    complete: true,
                },
                Err(SourceStoreError::Cancelled) => {
                    return Err(BaselineGap::Capture(EvidenceGap::Cancelled));
                }
                Err(_) => return Err(BaselineGap::Capture(EvidenceGap::Incomplete)),
            };
            // The stamp the listing took before it read the directory: a
            // change between the check above and the listing shows here.
            if !directory.proves(listing.stamp.as_ref()) {
                return Err(changed(directory));
            }
            capture.listed(&directory.path, &listing, false);
            observations.push(ResolutionObservation::Directory {
                path: directory.path.clone(),
                entry,
                stamp: listing.stamp.clone(),
                complete: listing.complete,
                listed: true,
            });
        }
        let captured = capture_evidence(
            &observations,
            &capture.snapshot(),
            &state.context_observations,
            &cancel,
        )
        .map_err(BaselineGap::Capture)?;
        Ok((discovery_scope(&directories), captured))
    }

    /// No member is pending except those retrying a previous refusal.
    fn index_settled(&self, key: &ContextKey) -> bool {
        self.project_cache
            .read_index(key, |index, _| {
                index
                    .pending(usize::MAX)
                    .into_iter()
                    .filter(|path| !index.retrying(path))
                    .collect::<Vec<_>>()
            })
            .is_some_and(|pending| pending.iter().all(|path| self.is_open_member(path)))
    }

    /// Seal when due (§6.3 amendment); `check` runs between store batches
    /// (gate, cancellation) and an `Err` retires the build.
    #[cfg_attr(not(test), allow(dead_code))] // the warmer (l4d-zf1b.8)
    pub(crate) fn seal_baseline_if_due(
        &self,
        key: &ContextKey,
        check: &mut dyn FnMut() -> Result<(), String>,
    ) -> SealOutcome {
        self.seal_baseline_at(key, Instant::now(), check)
    }

    /// `seal_baseline_if_due` at `now`, the clock the 60 s throttle reads.
    pub(crate) fn seal_baseline_at(
        &self,
        key: &ContextKey,
        now: Instant,
        check: &mut dyn FnMut() -> Result<(), String>,
    ) -> SealOutcome {
        let Some(state) = self.contexts.get(key) else {
            return SealOutcome::NotDue;
        };
        if state.context.project_file().is_none() {
            return SealOutcome::Ineligible(Ineligible::NotAProject);
        }
        if !self.persistent_cache().is_enabled()
            || !self
                .project_cache
                .baseline_seal_due(key, now, SEAL_INTERVAL)
            || !self.index_settled(key)
        {
            return SealOutcome::NotDue;
        }
        // The membership is checked before any directory is listed; the
        // record list lives until the seal is done.
        let members = match self.baseline_records(key) {
            Ok(records) => records.len(),
            Err(ineligible) => return SealOutcome::Ineligible(ineligible),
        };
        let Some(_records) = self.project_cache.reserve(
            ReservationKind::Scratch,
            members.saturating_mul(2 * std::mem::size_of::<Digest>()),
        ) else {
            return SealOutcome::NotDue;
        };
        let discovery = match self.discovery_evidence(key, check) {
            Ok(discovery) => discovery,
            Err(BaselineGap::NotAProject) => {
                return SealOutcome::Ineligible(Ineligible::NotAProject);
            }
            // `check` stopped the listing: retried after the next batch.
            Err(BaselineGap::Capture(EvidenceGap::Cancelled)) => {
                return SealOutcome::Interrupted;
            }
            Err(gap) => return SealOutcome::Gap(gap),
        };
        let eligible = match self.eligible_baseline_snapshot(key, discovery) {
            Ok(eligible) => eligible,
            Err(ineligible) => return SealOutcome::Ineligible(ineligible),
        };
        let records = record_list(eligible.value().records());
        let previous = self.project_cache.baseline_hint(key);
        if previous.as_ref().is_some_and(|hint| {
            hint.records == records
                && hint.header.discovery_evidence() == Some(eligible.value().discovery())
        }) {
            // A hold refused before (`WalPressure`) is asked for once more.
            if let Some(hint) = previous.filter(|hint| !hint.held)
                && let Some(job) = self.persistent_cache().job()
            {
                match job.store().hold_snapshot(&hint.header) {
                    Ok(true) => {
                        self.install_hint(&job, key, BaselineHint { held: true, ..hint });
                    }
                    Ok(false) => {}
                    Err(bypass) => job.observe(&bypass),
                }
            }
            self.project_cache.note_baseline_seal(key, now);
            return SealOutcome::Unchanged;
        }
        let Some(job) = self.persistent_cache().job() else {
            return SealOutcome::NotDue;
        };
        let mut gated = || {
            if job.is_cancelled() {
                return Err("the store is fenced".to_owned());
            }
            check()
        };
        let header = match seal_baseline(job.store(), &eligible, &mut gated) {
            Ok(header) => header,
            Err(SealFailure::Interrupted) => return SealOutcome::Interrupted,
            Err(SealFailure::Bypass(bypass)) => {
                job.observe(&bypass);
                return SealOutcome::Bypassed;
            }
        };
        let held = match job.store().hold_snapshot(&header) {
            Ok(held) => held,
            Err(bypass) => {
                job.observe(&bypass);
                false
            }
        };
        self.install_hint(
            &job,
            key,
            BaselineHint {
                header,
                records,
                held,
                sealed: true,
            },
        );
        self.project_cache.note_baseline_seal(key, now);
        SealOutcome::Sealed
    }

    /// Make `hint` `key`'s baseline and release the hold of the one it
    /// replaces; `false` (its own hold released) when `key` is no longer
    /// indexed.
    fn install_hint(&self, job: &CacheJob, key: &ContextKey, hint: BaselineHint) -> bool {
        let (id, held) = (hint.header.id(), hint.held);
        match self.project_cache.set_baseline_hint(key, hint) {
            Some(Some(old)) => {
                if old.held && old.header.id() != id {
                    release(job, old.header.id());
                }
                true
            }
            Some(None) => true,
            None => {
                if held {
                    release(job, id);
                }
                false
            }
        }
    }

    /// At the first batch of a context in this process: newest sealed
    /// candidate whose discovery evidence validates, held.
    #[cfg_attr(not(test), allow(dead_code))] // the startup crawl (l4d-zf1b.8)
    pub(crate) fn adopt_baseline_hint(&self, key: &ContextKey, cancel: &AtomicBool) -> Option<()> {
        matches!(self.adopt(key, cancel), Adoption::Adopted).then_some(())
    }

    /// The automatic adoption (M4): once per stored index, unless the
    /// attempt was cut short by something that may pass.
    pub(super) fn adopt_baseline_hint_once(&self, key: &ContextKey, cancel: &AtomicBool) {
        if self.project_cache.baseline_adoption_tried(key) {
            return;
        }
        if !matches!(self.adopt(key, cancel), Adoption::Retry) {
            self.project_cache.note_baseline_adoption_tried(key);
        }
    }

    fn adopt(&self, key: &ContextKey, cancel: &AtomicBool) -> Adoption {
        if self.project_cache.baseline_hint(key).is_some() {
            return Adoption::Adopted;
        }
        if !self.is_baseline() {
            return Adoption::None;
        }
        let Some(state) = self.contexts.get(key) else {
            return Adoption::None;
        };
        let Ok(directories) = self.membership_directories(key) else {
            return Adoption::None;
        };
        let scope = discovery_scope(&directories);
        let Some(job) = self.persistent_cache().job() else {
            return Adoption::None;
        };
        let store = job.store();
        let context = CacheContextKey::new(&state.context);
        let candidates = match store.find_snapshots(&context) {
            Ok(Lookup::Hit(candidates)) => candidates,
            Ok(Lookup::Miss(_)) => return Adoption::None,
            Err(bypass) => {
                job.observe(&bypass);
                return Adoption::Retry;
            }
        };
        let overlays = self.overlay_inputs();
        let deleted = super::resolver::deletion_overrides_by_path(&self.deleted_overrides);
        let reconciliation = super::ReconciliationBudget::new(Arc::new(AtomicBool::new(false)));
        // The store's fence through the validator's flag, the request's
        // cancellation through its budget.
        let work = RequestCancel {
            work: &reconciliation,
            cancel,
        };
        let mut validator = Validator::new(
            CurrentInputs {
                context: &context,
                read_policy: state.context.read_policy(),
                overlays: &overlays,
                deleted: &deleted,
                cache: &self.project_cache,
                max_file_bytes: self.options.limits.max_file_bytes,
                directory_entries: ResolverLimits::default().max_directory_entries,
            },
            &work,
            job.cancel_flag(),
        );
        for header in candidates {
            if job.is_cancelled() || cancel.load(Ordering::Relaxed) {
                return Adoption::Retry;
            }
            let Some(discovery) = header.discovery_evidence() else {
                continue;
            };
            let evidence = match store.open_evidence(discovery) {
                Ok(Lookup::Hit(evidence)) => evidence,
                Ok(Lookup::Miss(_)) => continue,
                Err(bypass) => {
                    job.observe(&bypass);
                    return Adoption::Retry;
                }
            };
            if evidence.purpose() != EvidencePurpose::Discovery
                || evidence.subject().subject != scope.digest()
            {
                continue;
            }
            match validate_evidence(store, &evidence, &mut validator) {
                Ok(EvidenceCheck::Holds) => {}
                Ok(EvidenceCheck::Stale(_) | EvidenceCheck::Store(_)) => continue,
                Ok(EvidenceCheck::Unproven(_)) => return Adoption::Retry,
                Err(bypass) => {
                    job.observe(&bypass);
                    return Adoption::Retry;
                }
            }
            let records = match snapshot_records(store, &header) {
                Ok(Some(records)) => records,
                Ok(None) => continue,
                Err(bypass) => {
                    job.observe(&bypass);
                    return Adoption::Retry;
                }
            };
            let held = match store.hold_snapshot(&header) {
                Ok(true) => true,
                // Retired since it was found.
                Ok(false) => continue,
                Err(bypass) => {
                    job.observe(&bypass);
                    false
                }
            };
            let hint = BaselineHint {
                header,
                records,
                held,
                sealed: false,
            };
            return if self.install_hint(&job, key, hint) {
                Adoption::Adopted
            } else {
                Adoption::None
            };
        }
        Adoption::None
    }

    /// Release the holds of evicted contexts' snapshots (D40).
    pub(super) fn release_evicted_holds(&self) {
        let Some(job) = self.persistent_cache().job() else {
            return;
        };
        for id in self.project_cache.take_released_holds() {
            release(&job, id);
        }
    }

    /// `path`'s facts through the record `hint` lists for it: the record
    /// is authorized for `key`'s context, then its member evidence (or, for
    /// a local artifact, the key from the current root bytes) is validated
    /// before the facts are loaded by ID (D31). This skips the
    /// evidence-candidate search, never the content validation (D38).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn reuse_from_hint(
        &self,
        uri: &Url,
        path: &Path,
        key: &ContextKey,
        hint: &BaselineHint,
        store: &Store,
        work: &dyn ProjectWorkBudget,
        decode_limit: usize,
        cancel: &AtomicBool,
    ) -> Result<HintReuse, Bypass> {
        let Some(state) = self.contexts.get(key) else {
            return Ok(HintReuse::Fallback);
        };
        if self.root_is_session_private(uri) {
            return Ok(HintReuse::Fallback);
        }
        let member = match store.find_member(&hint.header, path)? {
            Lookup::Hit(member) => member,
            Lookup::Miss(Miss::Retired | Miss::Vanished) => return Ok(HintReuse::Gone),
            Lookup::Miss(_) => return Ok(HintReuse::Fallback),
        };
        let record = match store.open_member_record(member.record_id)? {
            Lookup::Hit(record) => record,
            Lookup::Miss(_) => return Ok(HintReuse::Fallback),
        };
        let Ok(record) = authorize_record(record, &state.context) else {
            return Ok(HintReuse::Fallback);
        };
        let header = record.value().member();
        let Some(artifact) = header.artifact else {
            return Ok(HintReuse::Retrying);
        };
        let Ok(bytes) = self.saved_root_bytes(uri, &state.context) else {
            return Ok(HintReuse::Fallback);
        };
        let root_hash = pascal_project::content_hash_bytes(&bytes);
        let local = LocalArtifactKey::new(
            &SourceContent::from_bytes(&bytes),
            &super::conditional_context_for_uri(&state.context, uri),
            state.context.source_origin(path),
        );
        drop(bytes);
        let (evidence, include_stamps, reads) = match header.member_evidence {
            None if local.digest() == artifact => (None, Vec::new(), 0),
            None => return Ok(HintReuse::Fallback),
            Some(id) => {
                let evidence = match store.open_evidence(id)? {
                    Lookup::Hit(evidence) => evidence,
                    Lookup::Miss(_) => return Ok(HintReuse::Fallback),
                };
                // The evidence of this root's bytes for this artifact.
                if evidence.purpose() != EvidencePurpose::Artifact
                    || evidence.subject().subject != local.digest()
                    || evidence.artifact_id() != Some(artifact)
                {
                    return Ok(HintReuse::Fallback);
                }
                let overlays = self.overlay_inputs();
                let deleted = super::resolver::deletion_overrides_by_path(&self.deleted_overrides);
                let context = CacheContextKey::new(&state.context);
                let mut validator = Validator::new(
                    CurrentInputs {
                        context: &context,
                        read_policy: state.context.read_policy(),
                        overlays: &overlays,
                        deleted: &deleted,
                        cache: &self.project_cache,
                        max_file_bytes: self.options.limits.max_file_bytes,
                        directory_entries: ResolverLimits::default().max_directory_entries,
                    },
                    work,
                    cancel,
                );
                match validate_evidence(store, &evidence, &mut validator)? {
                    EvidenceCheck::Holds => {}
                    EvidenceCheck::Unproven(unproven) => {
                        return Ok(HintReuse::Done(Reuse::Miss(ReuseMiss::Unproven(unproven))));
                    }
                    EvidenceCheck::Stale(_) | EvidenceCheck::Store(_) => {
                        return Ok(HintReuse::Fallback);
                    }
                }
                let stamps = validator
                    .held_payloads()
                    .iter()
                    .filter(|(held, _)| !super::resolver::path_equivalent(held, path))
                    .cloned()
                    .collect();
                (Some(id), stamps, validator.reads())
            }
        };
        let Some(charge) = self
            .project_cache
            .reserve(ReservationKind::Facts, decode_limit)
        else {
            return Ok(HintReuse::Done(Reuse::Miss(ReuseMiss::Unproven(
                super::validate::Unproven::Memory,
            ))));
        };
        let mut budget = FactBudget::new(decode_limit);
        let loaded = match decoded(store.load_facts_by_id(artifact, &mut budget)) {
            Ok(loaded) => loaded,
            Err(PersistError::Store(bypass)) => return Err(bypass),
            Err(_) => return Ok(HintReuse::Fallback),
        };
        Ok(match loaded {
            Ok(Lookup::Hit(facts)) => {
                #[cfg(test)]
                HINT_REUSES.with(|count| count.set(count.get() + 1));
                HintReuse::Done(Reuse::Hit {
                    facts: admitted(facts, &budget, charge),
                    via: ReuseVia::Snapshot,
                    reads,
                    artifact,
                    evidence,
                    include_stamps,
                    root_hash,
                })
            }
            Ok(Lookup::Miss(_)) => HintReuse::Fallback,
            Err(miss) => HintReuse::Done(Reuse::Miss(miss)),
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::persistent_cache::PersistentCache;
    use crate::project_cache::{IndexAnchor, MAX_INDEXED_CONTEXTS};
    use crate::workspace::eligible::Ineligible;
    use crate::workspace::indexing::tests::persisting::{persisting_view, records, restarted_view};
    use crate::workspace::indexing::tests::{
        advance, indexed_project, members, named_unit, selected, workspace, write,
    };
    use crate::workspace::indexing::{IndexBound, IndexMode};
    use crate::workspace::persist::tests::private_dir;
    use crate::workspace::rename::CANCELLATION_MESSAGE;
    use crate::workspace::{ContextKey, Workspace, WorkspaceOptions};
    use lsp_types::Url;
    use pascal_cache::{ContextKey as CacheContextKey, Lookup, SnapshotHeader, SnapshotId};
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    fn seal(view: &Workspace, key: &ContextKey) -> SealOutcome {
        view.seal_baseline_if_due(key, &mut || Ok(()))
    }

    fn snapshots(
        view: &Workspace,
        cache: &PersistentCache,
        key: &ContextKey,
    ) -> Vec<SnapshotHeader> {
        let context = CacheContextKey::new(&view.contexts[key].context);
        match cache
            .job()
            .unwrap()
            .store()
            .find_snapshots(&context)
            .unwrap()
        {
            Lookup::Hit(headers) => headers,
            Lookup::Miss(_) => Vec::new(),
        }
    }

    fn held(cache: &PersistentCache) -> Vec<SnapshotId> {
        cache
            .job()
            .unwrap()
            .store()
            .held_snapshots_for_test()
            .unwrap()
    }

    fn hint_id(view: &Workspace, key: &ContextKey) -> Option<SnapshotId> {
        view.project_cache
            .baseline_hint(key)
            .map(|hint| hint.header().id())
    }

    /// A first process indexes `count` members, seals them and exits.
    fn sealed_first_session(
        root: &Path,
        directory: &Path,
        count: usize,
    ) -> (ContextKey, IndexAnchor, BaselineHint) {
        let (workspace, key, anchor) = indexed_project(root, count, WorkspaceOptions::default());
        let (mut first, cache) = persisting_view(&workspace, directory);
        let progress = advance(&mut first, &anchor, IndexBound::all());
        assert_eq!(progress.persisted, count, "{progress:?}");
        assert_eq!(seal(&first, &key), SealOutcome::Sealed);
        let hint = first.project_cache.baseline_hint(&key).unwrap();
        cache.shutdown();
        (key, anchor, hint)
    }

    fn reset_counters() {
        reset_hint_reuses_for_test();
        crate::workspace::persist::reset_reuse_in_context_calls_for_test();
    }

    #[test]
    fn a_complete_project_membership_is_sealed_once_and_held() {
        let temp = tempfile::tempdir().unwrap();
        let store = private_dir();
        let (workspace, key, anchor) =
            indexed_project(temp.path(), 10, WorkspaceOptions::default());
        let (mut view, cache) = persisting_view(&workspace, store.path());
        let progress = advance(&mut view, &anchor, IndexBound::all());
        assert!(progress.complete, "{progress:?}");
        assert_eq!(progress.persisted, 10);
        let now = Instant::now();
        assert_eq!(
            view.seal_baseline_at(&key, now, &mut || Ok(())),
            SealOutcome::Sealed
        );
        let candidates = snapshots(&view, &cache, &key);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].member_count(), 10);
        let hint = view.project_cache.baseline_hint(&key).unwrap();
        assert!(hint.held() && hint.sealed());
        assert_eq!(hint.header().id(), candidates[0].id());
        assert_eq!(held(&cache), vec![candidates[0].id()]);
        assert_eq!(
            view.seal_baseline_at(&key, now + Duration::from_secs(1), &mut || Ok(())),
            SealOutcome::NotDue
        );
        assert_eq!(
            view.seal_baseline_at(&key, now + Duration::from_secs(61), &mut || Ok(())),
            SealOutcome::Unchanged
        );
        assert_eq!(snapshots(&view, &cache, &key).len(), 1);
        assert_eq!(held(&cache), vec![candidates[0].id()]);
        cache.shutdown();
    }

    #[test]
    fn an_open_document_does_not_block_the_baseline_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let store = private_dir();
        let (mut workspace, key, anchor) =
            indexed_project(temp.path(), 4, WorkspaceOptions::default());
        let u0 = temp.path().join("lib/U0.pas");
        workspace
            .open_document(
                Url::from_file_path(&u0).unwrap(),
                "unit U0;\ninterface\nprocedure Edited;\nimplementation\nprocedure Edited;\nbegin\nend;\nend.\n"
                    .to_owned(),
                1,
            )
            .unwrap();
        let (mut view, cache) = persisting_view(&workspace, store.path());
        let progress = advance(&mut view, &anchor, IndexBound::all());
        assert_eq!(progress.persisted, 4, "{progress:?}");
        assert_eq!(seal(&view, &key), SealOutcome::Sealed);
        assert_eq!(selected(&view, &key, "P0"), vec![u0.clone()], "disk facts");
        // The same members indexed by a process that never opened U0.
        let reference_store = private_dir();
        let (mut reference, reference_cache) =
            persisting_view(&workspace_of(temp.path()), reference_store.path());
        advance(&mut reference, &anchor, IndexBound::all());
        assert_eq!(records(&view, &key), records(&reference, &key));
        let u0_record = records(&view, &key)
            .into_iter()
            .find(|(path, _)| *path == u0)
            .and_then(|(_, record)| record)
            .unwrap();
        let header = view
            .project_cache
            .baseline_hint(&key)
            .unwrap()
            .header()
            .clone();
        let Lookup::Hit(member) = cache
            .job()
            .unwrap()
            .store()
            .find_member(&header, &u0)
            .unwrap()
        else {
            panic!("U0 is a member of the snapshot")
        };
        assert_eq!(member.record_id, u0_record);
        reference_cache.shutdown();
        cache.shutdown();
    }

    fn workspace_of(root: &Path) -> Workspace {
        workspace(root, WorkspaceOptions::default())
    }

    #[test]
    fn an_empty_membership_is_never_sealed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let store = private_dir();
        write(
            &root.join("App.dproj"),
            "<Project><PropertyGroup><DCC_UnitSearchPath>lib</DCC_UnitSearchPath></PropertyGroup></Project>",
        );
        std::fs::create_dir_all(root.join("lib")).unwrap();
        pascal_project::backdate_tree_for_test(root);
        let (mut view, cache) = persisting_view(&workspace_of(root), store.path());
        let anchor = IndexAnchor::Project(root.join("App.dproj"));
        let progress = advance(&mut view, &anchor, IndexBound::all());
        let key = progress.key.clone().unwrap();
        assert!(members(&view, &key).is_empty(), "{progress:?}");
        assert_eq!(
            seal(&view, &key),
            SealOutcome::Ineligible(Ineligible::EmptyMembership)
        );
        assert!(snapshots(&view, &cache, &key).is_empty());
        assert!(held(&cache).is_empty());
        cache.shutdown();
    }

    #[test]
    fn an_incomplete_membership_is_never_sealed() {
        let temp = tempfile::tempdir().unwrap();
        let store = private_dir();
        let (workspace, key, anchor) = indexed_project(temp.path(), 3, WorkspaceOptions::default());
        let (mut view, cache) = persisting_view(&workspace, store.path());
        view.project_cache.set_index_unit_limit_for_test(2);
        let progress = advance(&mut view, &anchor, IndexBound::all());
        assert!(progress.complete, "{progress:?}");
        assert_eq!(members(&view, &key).len(), 2);
        assert_eq!(
            seal(&view, &key),
            SealOutcome::Ineligible(Ineligible::IncompleteMembership)
        );
        assert!(snapshots(&view, &cache, &key).is_empty());
        cache.shutdown();
    }

    #[test]
    fn a_member_without_a_proven_record_blocks_the_seal() {
        let temp = tempfile::tempdir().unwrap();
        let store = private_dir();
        let (workspace, key, anchor) = indexed_project(temp.path(), 4, WorkspaceOptions::default());
        let (mut view, cache) = persisting_view(&workspace, store.path());
        assert_eq!(advance(&mut view, &anchor, IndexBound::all()).persisted, 4);
        let member = temp.path().join("lib/U1.pas");
        let stamps = view
            .project_cache
            .read_index(&key, |index, _| index.stamps_of(&member).map(<[_]>::to_vec))
            .flatten()
            .unwrap();
        view.project_cache.mark_index_stale(&key, &member);
        assert!(
            view.project_cache
                .refresh_index_member(&key, &member, stamps)
        );
        assert!(!view.project_cache.index_member_has_record(&key, &member));
        assert_eq!(
            seal(&view, &key),
            SealOutcome::Ineligible(Ineligible::Unproven(member.clone()))
        );
        assert!(snapshots(&view, &cache, &key).is_empty());
        // The next batch proves it again.
        view.project_cache.mark_index_stale(&key, &member);
        let progress = advance(&mut view, &anchor, IndexBound::all());
        assert_eq!(progress.reused, 1, "{progress:?}");
        assert_eq!(seal(&view, &key), SealOutcome::Sealed);
        assert_eq!(snapshots(&view, &cache, &key)[0].member_count(), 4);
        cache.shutdown();
    }

    #[test]
    fn more_than_256_members_seal_in_batches() {
        let temp = tempfile::tempdir().unwrap();
        let store = private_dir();
        let (workspace, key, anchor) =
            indexed_project(temp.path(), 300, WorkspaceOptions::default());
        let (mut view, cache) = persisting_view(&workspace, store.path());
        let progress = advance(&mut view, &anchor, IndexBound::all());
        assert_eq!(progress.persisted, 300, "{progress:?}");
        let started = Instant::now();
        let outcome = seal(&view, &key);
        eprintln!("sealing 300 members took {:?}", started.elapsed());
        assert_eq!(outcome, SealOutcome::Sealed);
        let candidates = snapshots(&view, &cache, &key);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].member_count(), 300);
        let job = cache.job().unwrap();
        let mut reader = job.store().read_snapshot(&candidates[0]);
        let mut batches = Vec::new();
        while let Lookup::Hit(Some(batch)) = reader.next_batch().unwrap() {
            batches.push(batch.members().len());
        }
        assert_eq!(batches, [256, 44]);
        assert!(reader.is_verified());
        drop(job);
        cache.shutdown();
    }

    #[test]
    fn a_projectless_context_is_not_sealed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let store = private_dir();
        named_unit(&root.join("A.pas"), "PA");
        pascal_project::backdate_tree_for_test(root);
        let (mut view, cache) = persisting_view(&workspace_of(root), store.path());
        let anchor = IndexAnchor::Document(Url::from_file_path(root.join("A.pas")).unwrap());
        let progress = advance(&mut view, &anchor, IndexBound::all());
        let key = progress.key.clone().unwrap();
        assert_eq!(progress.persisted, 1, "{progress:?}");
        assert_eq!(
            view.discovery_evidence(&key, &mut || Ok(())).err(),
            Some(BaselineGap::NotAProject)
        );
        assert_eq!(
            seal(&view, &key),
            SealOutcome::Ineligible(Ineligible::NotAProject)
        );
        assert!(snapshots(&view, &cache, &key).is_empty());
        assert!(held(&cache).is_empty());
        cache.shutdown();
    }

    #[test]
    fn a_restart_adopts_the_snapshot_and_reuses_through_it() {
        let temp = tempfile::tempdir().unwrap();
        let store = private_dir();
        let (key, anchor, sealed) = sealed_first_session(temp.path(), store.path(), 12);
        let (mut second, cache) = restarted_view(temp.path(), store.path());
        reset_counters();
        crate::navigation::test_reset_document_parse_count();
        advance(
            &mut second,
            &anchor,
            IndexBound::units(0).with_mode(IndexMode::Reuse),
        );
        let hint = second.project_cache.baseline_hint(&key).expect("adopted");
        assert_eq!(hint.header().id(), sealed.header().id());
        assert_eq!(hint.records(), sealed.records(), "the same record list");
        assert!(hint.held() && !hint.sealed());
        assert_eq!(
            second.adopt_baseline_hint(&key, &AtomicBool::new(false)),
            Some(())
        );
        let progress = advance(
            &mut second,
            &anchor,
            IndexBound::all().with_mode(IndexMode::Reuse),
        );
        assert!(progress.complete, "{progress:?}");
        assert_eq!(progress.reused, 12);
        assert_eq!(crate::navigation::test_document_parse_count(), 0);
        assert_eq!(hint_reuses_for_test(), 12, "every member via the snapshot");
        assert_eq!(
            crate::workspace::persist::reuse_in_context_calls_for_test(),
            0,
            "no evidence-candidate search"
        );
        assert_eq!(seal(&second, &key), SealOutcome::Unchanged);
        assert_eq!(snapshots(&second, &cache, &key).len(), 1);
        assert_eq!(held(&cache), vec![sealed.header().id()]);
        cache.shutdown();
    }

    #[test]
    fn a_stale_snapshot_record_falls_back_and_reseals() {
        let temp = tempfile::tempdir().unwrap();
        let store = private_dir();
        let (key, anchor, old) = sealed_first_session(temp.path(), store.path(), 10);
        let edited = temp.path().join("lib/U2.pas");
        named_unit(&edited, "Renamed2");
        let (mut second, cache) = restarted_view(temp.path(), store.path());
        reset_counters();
        crate::navigation::test_reset_document_parse_count();
        let progress = advance(&mut second, &anchor, IndexBound::all());
        assert_eq!((progress.reused, progress.indexed), (9, 1), "{progress:?}");
        assert_eq!(progress.persisted, 1);
        assert_eq!(crate::navigation::test_document_parse_count(), 1);
        assert_eq!(hint_reuses_for_test(), 9);
        assert_eq!(
            crate::workspace::persist::reuse_in_context_calls_for_test(),
            1
        );
        assert_eq!(selected(&second, &key, "Renamed2"), vec![edited]);
        assert_eq!(seal(&second, &key), SealOutcome::Sealed);
        let candidates = snapshots(&second, &cache, &key);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[1].id(), old.header().id());
        assert_ne!(candidates[0].id(), old.header().id());
        assert_eq!(hint_id(&second, &key), Some(candidates[0].id()));
        assert_eq!(
            held(&cache),
            vec![candidates[0].id()],
            "the old hold released"
        );
        cache.shutdown();
    }

    /// H4/R8-3: a member the snapshot lists without an artifact is retried
    /// after every other member, and does not hold `complete` back.
    #[test]
    fn a_previous_refusal_is_retried_last() {
        let temp = tempfile::tempdir().unwrap();
        let store = private_dir();
        let mut options = WorkspaceOptions::default();
        options.limits.max_file_bytes = 300;
        let (session, key, anchor) = indexed_project(temp.path(), 3, options.clone());
        let big = temp.path().join("lib/U1.pas");
        named_unit(&big, &"Long".repeat(60));
        pascal_project::backdate_tree_for_test(temp.path());
        let (mut first, cache) = persisting_view(&session, store.path());
        let progress = advance(&mut first, &anchor, IndexBound::all());
        assert_eq!(
            (progress.refused, progress.persisted),
            (1, 2),
            "{progress:?}"
        );
        assert_eq!(seal(&first, &key), SealOutcome::Sealed);
        cache.shutdown();
        let (mut second, cache) = persisting_view(&workspace(temp.path(), options), store.path());
        let progress = advance(&mut second, &anchor, IndexBound::all());
        // The batch's spare bound retries it after every other member.
        assert_eq!(
            (
                progress.reused,
                progress.refused,
                progress.retries_attempted
            ),
            (2, 1, 1),
            "{progress:?}"
        );
        assert_eq!(progress.retrying, 0);
        assert!(
            progress.complete && !progress.worth_requeue(),
            "{progress:?}"
        );
        assert!(second.project_cache.index_member_has_record(&key, &big));
        assert_eq!(seal(&second, &key), SealOutcome::Unchanged);
        cache.shutdown();
    }

    /// A Full batch whose bound ends before the retried member asks to be
    /// requeued, and the next batch extracts it.
    #[test]
    fn a_retry_left_by_the_bound_requeues_the_batch() {
        let temp = tempfile::tempdir().unwrap();
        let store = private_dir();
        let mut options = WorkspaceOptions::default();
        options.limits.max_file_bytes = 300;
        let (session, key, anchor) = indexed_project(temp.path(), 2, options.clone());
        let big = temp.path().join("lib/U0.pas");
        named_unit(&big, &"Long".repeat(60));
        pascal_project::backdate_tree_for_test(temp.path());
        let (mut first, cache) = persisting_view(&session, store.path());
        assert_eq!(advance(&mut first, &anchor, IndexBound::all()).refused, 1);
        assert_eq!(seal(&first, &key), SealOutcome::Sealed);
        cache.shutdown();
        let (mut second, cache) = persisting_view(&workspace(temp.path(), options), store.path());
        let progress = advance(&mut second, &anchor, IndexBound::units(2));
        assert_eq!(
            (
                progress.reused,
                progress.retrying,
                progress.retries_attempted
            ),
            (1, 1, 0),
            "{progress:?}"
        );
        assert!(progress.complete, "a retry is no pending work");
        assert!(progress.worth_requeue(), "{progress:?}");
        let counts = second
            .project_cache
            .read_index(&key, |index, _| index.counts())
            .unwrap();
        assert_eq!((counts.retrying, counts.pending), (1, 1));
        let progress = advance(&mut second, &anchor, IndexBound::units(2));
        assert_eq!(
            (
                progress.refused,
                progress.retrying,
                progress.retries_attempted
            ),
            (1, 0, 1),
            "{progress:?}"
        );
        assert!(!progress.worth_requeue());
        assert!(second.project_cache.index_member_has_record(&key, &big));
        cache.shutdown();
    }

    /// A retried member a session view skips (an unsaved include overlay)
    /// keeps its retry, but does not count as attempted: the requeue ends.
    #[test]
    fn a_skipped_retry_does_not_requeue_forever() {
        let temp = tempfile::tempdir().unwrap();
        let store = private_dir();
        let mut options = WorkspaceOptions::default();
        options.limits.max_file_bytes = 300;
        let (session, key, anchor) = indexed_project(temp.path(), 2, options);
        let include = temp.path().join("lib/Shared.inc");
        write(&include, "const Shared = 1;\n");
        let big = temp.path().join("lib/U0.pas");
        write(
            &big,
            &format!(
                "unit U0;\ninterface\n{{$I Shared.inc}}\nprocedure {};\nimplementation\nend.\n",
                "Long".repeat(80)
            ),
        );
        pascal_project::backdate_tree_for_test(temp.path());
        let (mut first, cache) = persisting_view(&session, store.path());
        assert_eq!(advance(&mut first, &anchor, IndexBound::all()).refused, 1);
        assert_eq!(seal(&first, &key), SealOutcome::Sealed);
        cache.shutdown();
        // A new process with the default per-file limit: U0 would load now.
        let mut session = workspace_of(temp.path());
        let (mut view, cache) = persisting_view(&session, store.path());
        let progress = advance(
            &mut view,
            &anchor,
            IndexBound::all().with_mode(IndexMode::Reuse),
        );
        assert_eq!((progress.reused, progress.retrying), (1, 1), "{progress:?}");
        session
            .open_document(
                Url::from_file_path(&include).unwrap(),
                "const Shared = 2;\n".to_owned(),
                1,
            )
            .unwrap();
        for batch in 0..2 {
            let progress = advance(&mut session, &anchor, IndexBound::all());
            assert_eq!(
                (
                    progress.retrying,
                    progress.retries_attempted,
                    progress.indexed
                ),
                (1, 0, 0),
                "batch {batch}: {progress:?}"
            );
            assert!(progress.complete, "{progress:?}");
            assert!(!progress.worth_requeue(), "batch {batch}: {progress:?}");
        }
        cache.shutdown();
    }

    #[test]
    fn a_seal_cancelled_while_listing_is_interrupted() {
        let temp = tempfile::tempdir().unwrap();
        let store = private_dir();
        let (workspace, key, anchor) = indexed_project(temp.path(), 3, WorkspaceOptions::default());
        let (mut view, cache) = persisting_view(&workspace, store.path());
        advance(&mut view, &anchor, IndexBound::all());
        let mut checks = 0;
        let outcome = view.seal_baseline_if_due(&key, &mut || {
            checks += 1;
            Err(CANCELLATION_MESSAGE.to_owned())
        });
        assert_eq!(outcome, SealOutcome::Interrupted);
        assert_eq!(checks, 1, "before the first directory");
        assert!(snapshots(&view, &cache, &key).is_empty());
        assert_eq!(seal(&view, &key), SealOutcome::Sealed, "retried");
        cache.shutdown();
    }

    #[test]
    fn a_cancelled_discovery_or_adoption_decides_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let store = private_dir();
        let (key, anchor, sealed) = sealed_first_session(temp.path(), store.path(), 4);
        let (mut second, cache) = restarted_view(temp.path(), store.path());
        advance(
            &mut second,
            &anchor,
            IndexBound::units(0).with_mode(IndexMode::Reuse),
        );
        assert_eq!(
            second
                .discovery_evidence(&key, &mut || Err(CANCELLATION_MESSAGE.to_owned()))
                .err(),
            Some(BaselineGap::Capture(EvidenceGap::Cancelled))
        );
        let hint = second.project_cache.baseline_hint(&key).unwrap();
        second
            .project_cache
            .drop_baseline_hint(&key, hint.header().id());
        cache
            .job()
            .unwrap()
            .store()
            .release_snapshot(hint.header().id())
            .unwrap();
        assert_eq!(
            second.adopt_baseline_hint(&key, &AtomicBool::new(true)),
            None
        );
        assert_eq!(hint_id(&second, &key), None);
        assert_eq!(
            second.adopt_baseline_hint(&key, &AtomicBool::new(false)),
            Some(())
        );
        assert_eq!(hint_id(&second, &key), Some(sealed.header().id()));
        cache.shutdown();
    }

    #[test]
    fn an_unchanged_seal_holds_an_unheld_hint_again() {
        let temp = tempfile::tempdir().unwrap();
        let store = private_dir();
        let (workspace, key, anchor) = indexed_project(temp.path(), 3, WorkspaceOptions::default());
        let (mut view, cache) = persisting_view(&workspace, store.path());
        advance(&mut view, &anchor, IndexBound::all());
        let now = Instant::now();
        assert_eq!(
            view.seal_baseline_at(&key, now, &mut || Ok(())),
            SealOutcome::Sealed
        );
        let hint = view.project_cache.baseline_hint(&key).unwrap();
        let id = hint.header().id();
        assert!(cache.job().unwrap().store().release_snapshot(id).unwrap());
        view.project_cache.set_baseline_hint(
            &key,
            BaselineHint {
                held: false,
                ..hint
            },
        );
        assert!(held(&cache).is_empty());
        assert_eq!(
            view.seal_baseline_at(&key, now + Duration::from_secs(61), &mut || Ok(())),
            SealOutcome::Unchanged
        );
        assert_eq!(held(&cache), vec![id]);
        assert!(view.project_cache.baseline_hint(&key).unwrap().held());
        cache.shutdown();
    }

    #[test]
    fn a_retired_hint_snapshot_falls_back_to_evidence_lookup() {
        let temp = tempfile::tempdir().unwrap();
        let store = private_dir();
        let (key, anchor, sealed) = sealed_first_session(temp.path(), store.path(), 8);
        let (mut second, cache) = restarted_view(temp.path(), store.path());
        advance(
            &mut second,
            &anchor,
            IndexBound::units(0).with_mode(IndexMode::Reuse),
        );
        assert_eq!(hint_id(&second, &key), Some(sealed.header().id()));
        assert!(
            cache
                .job()
                .unwrap()
                .store()
                .retire_snapshot(sealed.header().id())
                .unwrap()
        );
        reset_counters();
        let progress = advance(
            &mut second,
            &anchor,
            IndexBound::all().with_mode(IndexMode::Reuse),
        );
        assert_eq!(progress.reused, 8, "{progress:?}");
        assert_eq!(hint_reuses_for_test(), 0);
        assert_eq!(
            crate::workspace::persist::reuse_in_context_calls_for_test(),
            8
        );
        assert_eq!(hint_id(&second, &key), None, "the hint is dropped");
        assert!(held(&cache).is_empty());
        cache.shutdown();
    }

    #[test]
    fn an_interrupted_seal_retires_its_build() {
        let temp = tempfile::tempdir().unwrap();
        let store = private_dir();
        let (workspace, key, anchor) = indexed_project(temp.path(), 6, WorkspaceOptions::default());
        let (mut view, cache) = persisting_view(&workspace, store.path());
        advance(&mut view, &anchor, IndexBound::all());
        // One check per listed directory (the project root and lib), one
        // before the only member batch, then one before the seal.
        let mut checks = 0;
        let outcome = view.seal_baseline_if_due(&key, &mut || {
            checks += 1;
            if checks > 3 {
                Err(CANCELLATION_MESSAGE.to_owned())
            } else {
                Ok(())
            }
        });
        assert_eq!(outcome, SealOutcome::Interrupted);
        assert_eq!(checks, 4, "stopped after the first batch");
        assert!(held(&cache).is_empty(), "no build is left held");
        assert!(snapshots(&view, &cache, &key).is_empty());
        assert_eq!(hint_id(&view, &key), None);
        assert_eq!(seal(&view, &key), SealOutcome::Sealed, "retried");
        cache.shutdown();
    }

    #[test]
    fn evicting_a_context_index_releases_its_hold() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let store = private_dir();
        let projects = MAX_INDEXED_CONTEXTS + 1;
        for index in 0..projects {
            write(
                &root.join(format!("p{index}/App.dproj")),
                "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup></Project>",
            );
            write(
                &root.join(format!("p{index}/App.dpr")),
                "program App; begin end.\n",
            );
        }
        pascal_project::backdate_tree_for_test(root);
        let (mut view, cache) = persisting_view(&workspace_of(root), store.path());
        let anchor = |index: usize| IndexAnchor::Project(root.join(format!("p{index}/App.dproj")));
        let mut sealed = Vec::new();
        for index in 0..projects {
            let progress = advance(&mut view, &anchor(index), IndexBound::all());
            let key = progress.key.clone().unwrap();
            assert_eq!(seal(&view, &key), SealOutcome::Sealed, "p{index}");
            sealed.push((key.clone(), hint_id(&view, &key).unwrap()));
            if index == projects - 2 {
                assert_eq!(held(&cache).len(), MAX_INDEXED_CONTEXTS);
            }
        }
        let (first_key, first_id) = sealed[0].clone();
        assert!(!view.project_cache.holds_index(&first_key), "evicted");
        let after = held(&cache);
        assert_eq!(after.len(), MAX_INDEXED_CONTEXTS, "{after:?}");
        assert!(!after.contains(&first_id), "released by the evicting batch");
        let mut expected = sealed[1..].iter().map(|(_, id)| *id).collect::<Vec<_>>();
        expected.sort();
        assert_eq!(after, expected);
        cache.shutdown();
    }

    #[test]
    fn a_changed_directory_listing_makes_the_membership_dirty_not_a_seal() {
        let temp = tempfile::tempdir().unwrap();
        let store = private_dir();
        let (workspace, key, anchor) = indexed_project(temp.path(), 4, WorkspaceOptions::default());
        let (mut view, cache) = persisting_view(&workspace, store.path());
        advance(&mut view, &anchor, IndexBound::all());
        assert!(view.discovery_evidence(&key, &mut || Ok(())).is_ok());
        let created = temp.path().join("lib/New.pas");
        named_unit(&created, "PNew");
        match view.discovery_evidence(&key, &mut || Ok(())) {
            Err(BaselineGap::Changed(directory)) => {
                assert!(directory.ends_with("lib"), "{directory:?}")
            }
            other => panic!("{:?}", other.err()),
        }
        assert!(!view.project_cache.index_membership_current(&key));
        let progress = advance(&mut view, &anchor, IndexBound::all());
        assert!(members(&view, &key).contains(&created));
        assert_eq!(progress.indexed, 1, "{progress:?}");
        cache.shutdown();
    }
}

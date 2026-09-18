//! LSP adapter for the request-scoped shared Pascal resolver.

use super::rename::{OverlayInput, SourceRecord, WorkspaceInput};
use super::{DiskStamp, MAX_DEPENDENCY_WORK, absolute_path, canonical_file_uri, path_stamp_result};
use lsp_types::Url;
use pascal_core::resolver::{
    CancellationToken, DirectoryListing, DirectoryRequest, FilesystemSourceStore, LegacyRoute,
    LoadedSource, ResolutionObservation, ResolutionReport, ResolverLimits, SourceId, SourceKind,
    SourceRequest, SourceRevision, SourceStore, SourceStoreError, UnitResolver,
};
use pascal_project::{
    ProjectContext, ProjectPathEntry, ProjectPathProvenance, ReadPolicy, content_hash_bytes,
};
#[cfg(test)]
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

#[cfg(test)]
thread_local! {
    static TEST_SOURCE_LOADS: RefCell<HashMap<PathBuf, usize>> = RefCell::new(HashMap::new());
}

#[cfg(test)]
pub(crate) fn reset_test_source_loads() {
    TEST_SOURCE_LOADS.with(|loads| loads.borrow_mut().clear());
}

#[cfg(test)]
pub(crate) fn test_source_load_count(path: &Path) -> usize {
    let path = absolute_path(path.to_path_buf());
    TEST_SOURCE_LOADS.with(|loads| loads.borrow().get(&path).copied().unwrap_or(0))
}

#[cfg(test)]
fn record_test_source_load(path: &Path) {
    let path = absolute_path(path.to_path_buf());
    TEST_SOURCE_LOADS.with(|loads| {
        let mut loads = loads.borrow_mut();
        *loads.entry(path).or_default() += 1;
    });
}

/// Source store used by LSP resolution.
///
/// Open documents are kept separately from the filesystem store so an overlay
/// wins even when the disk file exists and has a different revision.
#[derive(Debug, Clone)]
pub(crate) struct LspSourceStore {
    pub(crate) overlays: HashMap<PathBuf, OverlayInput>,
    pub(crate) paths_to_uris: HashMap<PathBuf, Url>,
    deleted_overrides: HashMap<PathBuf, Option<DiskStamp>>,
    rejected_paths: std::collections::HashSet<PathBuf>,
    context: ProjectContext,
    disk: FilesystemSourceStore,
}

pub(crate) fn decode_source_bytes(bytes: &[u8]) -> String {
    if let Some(bytes) = bytes.strip_prefix(&[0xff, 0xfe]) {
        return String::from_utf16_lossy(
            &bytes
                .chunks_exact(2)
                .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                .collect::<Vec<_>>(),
        );
    }
    if let Some(bytes) = bytes.strip_prefix(&[0xfe, 0xff]) {
        return String::from_utf16_lossy(
            &bytes
                .chunks_exact(2)
                .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
                .collect::<Vec<_>>(),
        );
    }
    pascal_core::decode_bytes(bytes).into_owned()
}

impl LspSourceStore {
    pub(crate) fn from_input(context: ProjectContext, input: &WorkspaceInput) -> Self {
        let mut overlays = HashMap::new();
        let mut paths_to_uris = HashMap::new();
        for (uri, overlay) in &input.overlays {
            let Ok(path) = uri.to_file_path() else {
                continue;
            };
            let path = absolute_path(path);
            let uri = canonical_file_uri(uri);
            overlays.insert(path.clone(), overlay.clone());
            paths_to_uris.insert(path, uri);
        }
        let deleted_overrides = input
            .deleted_overrides
            .iter()
            .filter_map(|(uri, stamp)| {
                uri.to_file_path()
                    .ok()
                    .map(|path| (absolute_path(path), stamp.clone()))
            })
            .collect();
        let rejected_paths = input
            .rejected_documents
            .iter()
            .filter_map(|uri| uri.to_file_path().ok().map(absolute_path))
            .collect();
        Self {
            overlays,
            paths_to_uris,
            deleted_overrides,
            rejected_paths,
            context,
            disk: FilesystemSourceStore::new(),
        }
    }

    pub(crate) fn uri_for_path(&self, path: &Path) -> Option<Url> {
        let path = absolute_path(path.to_path_buf());
        self.paths_to_uris
            .get(&path)
            .cloned()
            .or_else(|| Url::from_file_path(path).ok())
    }

    pub(crate) fn uri_for_source_id(&self, source_id: &SourceId) -> Option<Url> {
        source_id
            .as_str()
            .strip_prefix("source:")
            .and_then(|path| self.uri_for_path(Path::new(path)))
    }

    pub(crate) fn source_record(
        &self,
        source: &LoadedSource,
        include_payload: bool,
    ) -> Result<SourceRecord, String> {
        let uri = self
            .uri_for_path(&source.path)
            .ok_or_else(|| format!("could not create a file URI for {}", source.path.display()))?;
        source_record_for_loaded(&self.context, source, uri, include_payload)
    }

    pub(crate) fn load_path(
        &mut self,
        path: &Path,
        legacy_route: Option<&LegacyRoute>,
        max_bytes: usize,
        cancel: &dyn CancellationToken,
    ) -> Result<LoadedSource, SourceStoreError> {
        let path = absolute_path(path.to_path_buf());
        let entry = self
            .context
            .path_entry_for(&path)
            .or_else(|| {
                legacy_route.map(|_| ProjectPathEntry {
                    path: path.clone(),
                    provenance: ProjectPathProvenance::LegacyNative,
                })
            })
            .ok_or_else(|| SourceStoreError::Unauthorized {
                path: path.clone(),
                reason: "source is outside the requester-scoped project roots".to_string(),
            })?;
        let read_policy = self.context.read_policy.clone();
        self.load(
            SourceRequest {
                path: &path,
                entry: &entry,
                read_policy: &read_policy,
                legacy_route,
                kind: SourceKind::Unit,
                max_bytes,
            },
            cancel,
        )
    }

    fn overlay_entry(&self, path: &Path, requested: &ProjectPathEntry) -> Option<ProjectPathEntry> {
        self.context.path_entry_for(path).or_else(|| {
            path_equivalent(path, &requested.path).then(|| ProjectPathEntry {
                path: path.to_path_buf(),
                provenance: requested.provenance.clone(),
            })
        })
    }

    fn overlay_is_authorized(
        &self,
        _path: &Path,
        entry: &ProjectPathEntry,
        read_policy: &ReadPolicy,
        _legacy_route: Option<&LegacyRoute>,
    ) -> bool {
        (matches!(entry.provenance, ProjectPathProvenance::LegacyNative)
            && read_policy.allows_legacy_route_entry(entry))
            || read_policy.allows_location(entry)
    }

    fn overlay_children(&self, directory: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let directory = absolute_path(directory.to_path_buf());
        let mut files = Vec::new();
        let directories = Vec::new();
        for path in self.overlays.keys() {
            let Some(parent) = path.parent() else {
                continue;
            };
            if !path_equivalent(parent, &directory) {
                continue;
            }
            if self.context.path_entry_for(path).is_none() {
                continue;
            }
            files.push(path.clone());
        }
        files.sort_by_key(|left| path_key(left));
        (files, directories)
    }
}

impl SourceStore for LspSourceStore {
    fn list_directory(
        &mut self,
        request: DirectoryRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<DirectoryListing, SourceStoreError> {
        let overlay_children = self.overlay_children(request.directory);
        match self.disk.list_directory(request, cancel) {
            Ok(mut listing) => {
                for path in overlay_children.0 {
                    if !listing
                        .files
                        .iter()
                        .any(|existing| path_equivalent(existing, &path))
                    {
                        listing.files.push(path);
                    }
                }
                listing.files.sort_by_key(|left| path_key(left));
                Ok(listing)
            }
            Err(SourceStoreError::NotFound { path }) if !overlay_children.0.is_empty() => {
                Ok(DirectoryListing {
                    files: overlay_children.0,
                    directories: overlay_children.1,
                    stamp: path_stamp_result(&path).ok().flatten(),
                    complete: true,
                })
            }
            Err(error) => Err(error),
        }
    }

    fn overlay_candidates(&self, roots: &[PathBuf], names: &[String]) -> Vec<PathBuf> {
        let mut paths = self
            .overlays
            .keys()
            .filter(|path| {
                roots.iter().any(|root| path_starts_with(path, root))
                    && path.file_name().is_some_and(|file_name| {
                        names
                            .iter()
                            .any(|name| file_name.to_string_lossy().eq_ignore_ascii_case(name))
                    })
            })
            .cloned()
            .collect::<Vec<_>>();
        paths.sort_by_key(|left| path_key(left));
        paths.dedup_by(|left, right| path_equivalent(left, right));
        paths
    }

    fn load(
        &mut self,
        request: SourceRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<LoadedSource, SourceStoreError> {
        if cancel.is_cancelled() {
            return Err(SourceStoreError::Cancelled);
        }
        #[cfg(test)]
        record_test_source_load(request.path);
        let path = absolute_path(request.path.to_path_buf());
        if self.rejected_paths.contains(&path) {
            return Err(SourceStoreError::Unauthorized {
                path,
                reason: "open document was rejected by workspace limits".to_string(),
            });
        }
        if !path_equivalent(&path, request.entry.path.as_path()) {
            return Err(SourceStoreError::Unauthorized {
                path,
                reason: "source and authorization entry identify different paths".to_string(),
            });
        }

        if let Some(overlay) = self.overlays.get(&path) {
            let Some(entry) = self.overlay_entry(&path, request.entry) else {
                return Err(SourceStoreError::Unauthorized {
                    path,
                    reason: "overlay is outside the requester-scoped project roots".to_string(),
                });
            };
            if !self.overlay_is_authorized(&path, &entry, request.read_policy, request.legacy_route)
            {
                return Err(SourceStoreError::Unauthorized {
                    path,
                    reason: "overlay is outside the requester-scoped read policy".to_string(),
                });
            }
            if overlay.text.len() > request.max_bytes {
                return Err(SourceStoreError::TooLarge {
                    path,
                    maximum: request.max_bytes,
                });
            }
            return Ok(LoadedSource {
                id: source_id_for_path(&path),
                path,
                bytes: Arc::<[u8]>::from(overlay.text.as_bytes().to_vec()),
                decoded_text: Some(Arc::<str>::from(overlay.text.clone())),
                revision: SourceRevision::Overlay {
                    version: overlay.version,
                    content_hash: content_hash_bytes(overlay.text.as_bytes()),
                },
            });
        }

        if self
            .deleted_overrides
            .get(&path)
            .is_some_and(|stamp| super::disk_stamp(&path) == *stamp)
        {
            return Err(SourceStoreError::NotFound { path });
        }

        let Some(context_entry) = self.context.path_entry_for(&path) else {
            if request.legacy_route.is_none() {
                return Err(SourceStoreError::Unauthorized {
                    path,
                    reason: "disk source is outside the requester-scoped project roots".to_string(),
                });
            }
            return self
                .disk
                .load(
                    SourceRequest {
                        path: request.path,
                        entry: request.entry,
                        read_policy: request.read_policy,
                        legacy_route: request.legacy_route,
                        kind: request.kind,
                        max_bytes: request.max_bytes,
                    },
                    cancel,
                )
                .map(with_decoded_text);
        };
        let entry = ProjectPathEntry {
            path,
            provenance: request.entry.provenance.clone(),
        };
        let _context_entry_matches = path_equivalent(&context_entry.path, &entry.path);
        self.disk
            .load(
                SourceRequest {
                    path: &entry.path,
                    entry: &entry,
                    read_policy: request.read_policy,
                    legacy_route: request.legacy_route,
                    kind: request.kind,
                    max_bytes: request.max_bytes,
                },
                cancel,
            )
            .map(with_decoded_text)
    }
}

fn with_decoded_text(mut source: LoadedSource) -> LoadedSource {
    source.decoded_text = Some(Arc::<str>::from(decode_source_bytes(&source.bytes)));
    source
}

pub(crate) fn resolver_for_context(
    context: ProjectContext,
    roots: Vec<PathBuf>,
    input: &WorkspaceInput,
    _cancel: &AtomicBool,
) -> UnitResolver<LspSourceStore> {
    let mut limits = ResolverLimits::default();
    limits.max_dependency_units = MAX_DEPENDENCY_WORK;
    limits.max_source_bytes = input.options.limits.max_file_bytes;
    resolver_for_context_with_limits(context, roots, input, limits)
}

pub(crate) fn resolver_for_context_with_limits(
    context: ProjectContext,
    roots: Vec<PathBuf>,
    input: &WorkspaceInput,
    limits: ResolverLimits,
) -> UnitResolver<LspSourceStore> {
    UnitResolver::new(
        context.clone(),
        roots,
        LspSourceStore::from_input(context, input),
        limits,
    )
}

pub(crate) fn source_record_for_loaded(
    context: &ProjectContext,
    source: &LoadedSource,
    uri: Url,
    include_payload: bool,
) -> Result<SourceRecord, String> {
    let text = source
        .decoded_text
        .as_deref()
        .map_or_else(|| decode_source_bytes(&source.bytes), ToOwned::to_owned);
    match &source.revision {
        SourceRevision::Overlay {
            version,
            content_hash: _,
        } => Ok(SourceRecord {
            uri,
            text,
            version: Some(*version),
            stamp: None,
            open: true,
            path: None,
            path_stamp: None,
            // Worker snapshots revalidate overlays with their exact text.
            // The resolver's byte hash is intentionally not copied here:
            // rename's text hash has different Hash-trait framing, and an
            // overlay record must not fail revalidation merely because the
            // two hash implementations differ.
            content_hash: None,
            content_bytes: None,
            candidate_membership: None,
            read_policy: Some(context.read_policy.clone()),
            path_entry: context.path_entry_for(&source.path),
            include_payload,
            missing_provider_candidate: false,
            directory_observation: false,
        }),
        SourceRevision::Disk {
            stamp,
            content_hash,
            read_policy,
            path_entry,
        } => Ok(SourceRecord {
            uri,
            text,
            version: None,
            stamp: Some(DiskStamp {
                bytes: stamp.bytes,
                modified: stamp.modified,
            }),
            open: false,
            path: Some(source.path.clone()),
            path_stamp: Some(stamp.clone()),
            content_hash: Some(*content_hash),
            content_bytes: None,
            candidate_membership: None,
            read_policy: Some(read_policy.clone()),
            path_entry: Some(path_entry.clone()),
            include_payload,
            missing_provider_candidate: false,
            directory_observation: false,
        }),
    }
}

fn payload_record(
    context: &ProjectContext,
    path: &Path,
    revision: &SourceRevision,
) -> Option<SourceRecord> {
    let uri = Url::from_file_path(path).ok()?;
    match revision {
        SourceRevision::Disk {
            stamp,
            content_hash,
            read_policy,
            path_entry,
        } => Some(SourceRecord {
            uri,
            text: String::new(),
            version: None,
            stamp: Some(DiskStamp {
                bytes: stamp.bytes,
                modified: stamp.modified,
            }),
            open: false,
            path: Some(path.to_path_buf()),
            path_stamp: Some(stamp.clone()),
            content_hash: Some(*content_hash),
            content_bytes: None,
            candidate_membership: None,
            read_policy: Some(read_policy.clone()),
            path_entry: Some(path_entry.clone()),
            include_payload: false,
            missing_provider_candidate: false,
            directory_observation: false,
        }),
        SourceRevision::Overlay {
            version,
            content_hash,
        } => Some(SourceRecord {
            uri,
            text: String::new(),
            version: Some(*version),
            stamp: None,
            open: true,
            path: None,
            path_stamp: None,
            // This record has no decoded payload.  The revalidator recognizes
            // an empty open record as a byte-hash-only overlay observation.
            content_hash: Some(*content_hash),
            content_bytes: None,
            candidate_membership: None,
            read_policy: Some(context.read_policy.clone()),
            path_entry: context.path_entry_for(path),
            include_payload: false,
            missing_provider_candidate: false,
            directory_observation: false,
        }),
    }
}

pub(crate) fn observation_record(observation: &ResolutionObservation) -> Option<SourceRecord> {
    let (path, stamp, missing_provider_candidate, directory_observation) = match observation {
        ResolutionObservation::Directory {
            path,
            stamp,
            complete: _,
            entry: _,
        } => (path, stamp.clone(), false, true),
        ResolutionObservation::Candidate {
            path,
            stamp,
            present,
            entry: _,
        } => (path, stamp.clone(), !present, false),
        ResolutionObservation::Metadata(_) | ResolutionObservation::ProjectRead(_) => return None,
        ResolutionObservation::Payload { .. } => return None,
    };
    let uri = Url::from_file_path(path).ok()?;
    Some(SourceRecord {
        uri,
        text: String::new(),
        version: None,
        stamp: None,
        open: false,
        path: Some(path.clone()),
        path_stamp: stamp,
        content_hash: None,
        content_bytes: None,
        candidate_membership: None,
        read_policy: None,
        path_entry: None,
        include_payload: false,
        missing_provider_candidate,
        directory_observation,
    })
}

/// Convert resolver metadata reads into the path records consumed by LSP
/// snapshot and stale-result validation. Project-read observations retain the
/// bytes read at the parser boundary; metadata payload observations retain the
/// authorization used to reread them later.
pub(crate) fn report_records(
    context: &ProjectContext,
    report: &ResolutionReport,
) -> Vec<SourceRecord> {
    let project_reads = report
        .observations
        .iter()
        .filter_map(|observation| match observation {
            ResolutionObservation::ProjectRead(read) => Some(read),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut records = HashMap::<Url, SourceRecord>::new();

    for observation in &report.observations {
        if let ResolutionObservation::Payload { path, revision, .. } = observation {
            if let Some(record) = payload_record(context, path, revision) {
                merge_source_record(&mut records, record);
            }
            continue;
        }
        let (path, path_stamp, content_hash, content_bytes, read_policy, path_entry) =
            match observation {
                ResolutionObservation::Metadata(pascal_project::MetadataObservation::Stat {
                    path,
                }) => (
                    path.clone(),
                    path_stamp_result(path).ok().flatten(),
                    None,
                    None,
                    None,
                    None,
                ),
                ResolutionObservation::Metadata(pascal_project::MetadataObservation::Payload {
                    path,
                    read_policy,
                    path_entry,
                    stamp,
                    content_hash,
                }) => {
                    let read = project_reads
                        .iter()
                        .find(|read| path_equivalent(&read.path, path));
                    (
                        path.clone(),
                        read.map(|read| read.stamp.clone())
                            .or_else(|| stamp.clone()),
                        Some(*content_hash),
                        read.and_then(|read| read.content_bytes.clone()),
                        Some(read_policy.clone()),
                        Some(path_entry.clone()),
                    )
                }
                ResolutionObservation::ProjectRead(read) => {
                    if report.observations.iter().any(|other| {
                        matches!(
                            other,
                            ResolutionObservation::Metadata(
                                pascal_project::MetadataObservation::Payload { path, .. }
                            ) if path_equivalent(path, &read.path)
                        )
                    }) {
                        continue;
                    }
                    (
                        read.path.clone(),
                        Some(read.stamp.clone()),
                        Some(read.content_hash),
                        read.content_bytes.clone(),
                        Some(context.read_policy.clone()),
                        context.path_entry_for(&read.path),
                    )
                }
                _ => continue,
            };
        let Some(uri) = Url::from_file_path(&path).ok() else {
            continue;
        };
        let record = SourceRecord {
            uri: uri.clone(),
            text: String::new(),
            version: None,
            stamp: None,
            open: false,
            path: Some(path),
            path_stamp,
            content_hash,
            content_bytes,
            candidate_membership: None,
            read_policy,
            path_entry,
            include_payload: false,
            missing_provider_candidate: false,
            directory_observation: false,
        };
        if let Some(existing) = records.get_mut(&uri) {
            if existing.path_stamp.is_none() {
                existing.path_stamp = record.path_stamp.clone();
            }
            if existing.content_hash.is_none() {
                existing.content_hash = record.content_hash;
            }
            if existing.content_bytes.is_none() {
                existing.content_bytes = record.content_bytes.clone();
            }
            if existing.read_policy.is_none() {
                existing.read_policy = record.read_policy.clone();
            }
            if existing.path_entry.is_none() {
                existing.path_entry = record.path_entry.clone();
            }
        } else {
            records.insert(uri, record);
        }
    }

    records.into_values().collect()
}

pub(crate) fn merge_source_record(
    records: &mut HashMap<Url, SourceRecord>,
    incoming: SourceRecord,
) {
    let Some(existing) = records.get_mut(&incoming.uri) else {
        records.insert(incoming.uri.clone(), incoming);
        return;
    };
    if incoming.open && !existing.open {
        // An overlay is authoritative even when project metadata read the
        // same path from disk first.  Do not combine the disk revision with
        // the open document: that would make stale-result validation reject
        // every valid overlay computation.
        *existing = incoming;
        return;
    }
    if existing.text.is_empty() && !incoming.text.is_empty() {
        existing.text = incoming.text.clone();
    }
    if existing.open && !existing.text.is_empty() && incoming.content_hash.is_none() {
        // A full overlay record carries the authoritative decoded text.  Any
        // hash-only overlay record created from a resolver observation uses the
        // core byte-hash framing and must not survive that merge.
        existing.content_hash = None;
    }
    if existing.version.is_none() {
        existing.version = incoming.version;
    }
    if existing.stamp.is_none() {
        existing.stamp = incoming.stamp.clone();
    }
    if existing.path.is_none() && !existing.open {
        existing.path = incoming.path.clone();
    }
    if existing.path_stamp.is_none() {
        existing.path_stamp = incoming.path_stamp.clone();
    }
    // The core overlay hash is over raw bytes, while LSP's existing open
    // record is revalidated from the exact decoded document text.  Do not
    // replace that text-based record with a differently-framed byte hash.
    if existing.content_hash.is_none() && existing.text.is_empty() {
        existing.content_hash = incoming.content_hash;
    }
    if existing.content_bytes.is_none() {
        existing.content_bytes = incoming.content_bytes.clone();
    }
    if existing.candidate_membership.is_none() {
        existing.candidate_membership = incoming.candidate_membership.clone();
    }
    if existing.read_policy.is_none() {
        existing.read_policy = incoming.read_policy.clone();
    }
    if existing.path_entry.is_none() {
        existing.path_entry = incoming.path_entry.clone();
    }
    existing.open |= incoming.open;
    existing.include_payload |= incoming.include_payload;
    existing.missing_provider_candidate |= incoming.missing_provider_candidate;
    existing.directory_observation |= incoming.directory_observation;
}

pub(crate) fn merge_report_into_context(context: &mut ProjectContext, report: &ResolutionReport) {
    for observation in &report.observations {
        match observation {
            pascal_core::ResolutionObservation::Metadata(observation) => {
                pascal_project::add_metadata_observation(
                    &mut context.metadata_observations,
                    observation.clone(),
                );
                if !context
                    .metadata_files
                    .iter()
                    .any(|path| path_equivalent(path, observation.path()))
                {
                    context
                        .metadata_files
                        .push(observation.path().to_path_buf());
                }
            }
            pascal_core::ResolutionObservation::ProjectRead(observation) => {
                if !context
                    .metadata_files
                    .iter()
                    .any(|path| path_equivalent(path, &observation.path))
                {
                    context.metadata_files.push(observation.path.clone());
                }
            }
            pascal_core::ResolutionObservation::Directory { .. }
            | pascal_core::ResolutionObservation::Candidate { .. }
            | pascal_core::ResolutionObservation::Payload { .. } => {}
        }
    }
    for warning in &report.warnings {
        if !context.warnings.iter().any(|existing| existing == warning) {
            context.warnings.push(warning.clone());
        }
    }
}

fn source_id_for_path(path: &Path) -> SourceId {
    SourceId::new(format!(
        "source:{}",
        absolute_path(path.to_path_buf()).display()
    ))
}

fn path_key(path: &Path) -> String {
    let mut key = absolute_path(path.to_path_buf())
        .to_string_lossy()
        .replace('\\', "/");
    if cfg!(windows) {
        key = key.to_ascii_lowercase();
    }
    key
}

fn path_equivalent(left: &Path, right: &Path) -> bool {
    path_key(left) == path_key(right)
}

fn path_starts_with(path: &Path, root: &Path) -> bool {
    let path = absolute_path(path.to_path_buf());
    let root = absolute_path(root.to_path_buf());
    let path_components = path.components().collect::<Vec<_>>();
    let root_components = root.components().collect::<Vec<_>>();
    path_components.len() >= root_components.len()
        && path_components
            .iter()
            .zip(root_components.iter())
            .all(|(path, root)| {
                if cfg!(windows) {
                    path.as_os_str()
                        .to_string_lossy()
                        .eq_ignore_ascii_case(&root.as_os_str().to_string_lossy())
                } else {
                    path == root
                }
            })
}

//! LSP adapter for the request-scoped shared Pascal resolver.

use super::rename::{OverlayInput, ResolverCandidateObservation, SourceRecord, WorkspaceInput};
use super::{DiskStamp, MAX_DEPENDENCY_WORK, absolute_path, canonical_file_uri, path_stamp_result};
use lsp_types::Url;
use pascal_core::resolver::{
    CancellationToken, DirectoryListing, DirectoryRequest, FilesystemSourceStore, LegacyRoute,
    LoadedSource, ResolutionObservation, ResolutionReport, ResolverLimits, ResolverSessionCache,
    SourceId, SourceKind, SourceRequest, SourceRevision, SourceStore, SourceStoreError,
    UnitResolver,
};
use pascal_project::{
    ProjectContext, ProjectObservations, ProjectPathEntry, ProjectPathProvenance, ReadPolicy,
    content_hash_bytes,
};
#[cfg(test)]
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

#[cfg(test)]
thread_local! {
    static TEST_BYTE_WORK_CAP: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    static TEST_SOURCE_LOADS: RefCell<HashMap<PathBuf, usize>> = RefCell::new(HashMap::new());
    static TEST_DISK_READS: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
}

#[cfg(test)]
pub(super) struct TestByteWorkCap(Option<usize>);
#[cfg(test)]
impl Drop for TestByteWorkCap {
    fn drop(&mut self) {
        TEST_BYTE_WORK_CAP.with(|cap| cap.set(self.0));
    }
}
#[cfg(test)]
pub(super) fn test_byte_work_cap(cap: usize) -> TestByteWorkCap {
    TestByteWorkCap(TEST_BYTE_WORK_CAP.with(|value| value.replace(Some(cap))))
}
pub(super) fn byte_work_cap(production: usize) -> usize {
    #[cfg(test)]
    {
        TEST_BYTE_WORK_CAP.with(|value| value.get().map_or(production, |cap| cap.min(production)))
    }
    #[cfg(not(test))]
    {
        production
    }
}

/// Source payload reads from disk on this thread, in order.
#[cfg(test)]
pub(crate) fn take_test_disk_reads() -> Vec<PathBuf> {
    TEST_DISK_READS.with(|reads| std::mem::take(&mut *reads.borrow_mut()))
}

#[cfg(test)]
pub(crate) fn record_test_disk_read(path: &Path) {
    let path = absolute_path(path.to_path_buf());
    TEST_DISK_READS.with(|reads| reads.borrow_mut().push(path));
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
    context: Arc<ProjectContext>,
    project_cache: crate::project_cache::ProjectCache,
    disk: FilesystemSourceStore,
}

pub(crate) fn decode_source_bytes(bytes: &[u8]) -> String {
    if let Some(bytes) = bytes.strip_prefix(&[0xff, 0xfe]) {
        return String::from_utf16_lossy(
            &bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                .collect::<Vec<_>>(),
        );
    }
    if let Some(bytes) = bytes.strip_prefix(&[0xfe, 0xff]) {
        return String::from_utf16_lossy(
            &bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
                .collect::<Vec<_>>(),
        );
    }
    pascal_core::decode_bytes(bytes).into_owned()
}

fn encoding_admission_error(
    reason: pascal_core::compile_time::IncompleteReason,
) -> SourceStoreError {
    if matches!(
        reason,
        pascal_core::compile_time::IncompleteReason::Cancelled
    ) {
        SourceStoreError::Cancelled
    } else {
        SourceStoreError::AdmissionUnavailable
    }
}

fn decoded_utf16_chars(bytes: &[u8], little: bool) -> impl Iterator<Item = char> + '_ {
    std::char::decode_utf16(bytes.as_chunks::<2>().0.iter().map(move |unit| {
        if little {
            u16::from_le_bytes([unit[0], unit[1]])
        } else {
            u16::from_be_bytes([unit[0], unit[1]])
        }
    }))
    .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
}

fn admit_decoded_source(
    source: LoadedSource,
    budget: &mut pascal_core::compile_time::CompileTimeBudget<'_>,
    cancel: &dyn CancellationToken,
) -> Result<LoadedSource, SourceStoreError> {
    enum Encoding<'a> {
        Utf8(&'a str),
        Utf16(&'a [u8], bool),
        Latin1(&'a [u8]),
    }
    let poll = || {
        if cancel.is_cancelled() {
            Err(SourceStoreError::Cancelled)
        } else {
            Ok(())
        }
    };
    poll()?;
    let bytes = source.bytes.as_ref();
    budget
        .charge(1, bytes.len(), 0)
        .map_err(encoding_admission_error)?;
    let encoding = if let Some(bytes) = bytes.strip_prefix(&[0xff, 0xfe]) {
        Encoding::Utf16(bytes, true)
    } else if let Some(bytes) = bytes.strip_prefix(&[0xfe, 0xff]) {
        Encoding::Utf16(bytes, false)
    } else if let Ok(text) = std::str::from_utf8(bytes) {
        Encoding::Utf8(text)
    } else {
        Encoding::Latin1(bytes)
    };
    poll()?;
    let decoded_bytes = match &encoding {
        Encoding::Utf8(text) => text.len(),
        Encoding::Utf16(bytes, little) => {
            budget
                .charge(1, bytes.len(), 0)
                .map_err(encoding_admission_error)?;
            let mut length = 0usize;
            for (n, ch) in decoded_utf16_chars(bytes, *little).enumerate() {
                if n % 4096 == 0 {
                    poll()?;
                }
                length = length
                    .checked_add(ch.len_utf8())
                    .ok_or(SourceStoreError::AdmissionUnavailable)?;
            }
            length
        }
        Encoding::Latin1(bytes) => {
            budget
                .charge(1, bytes.len(), 0)
                .map_err(encoding_admission_error)?;
            let mut length = 0usize;
            for (n, byte) in bytes.iter().enumerate() {
                if n % 4096 == 0 {
                    poll()?;
                }
                length = length
                    .checked_add((*byte as char).len_utf8())
                    .ok_or(SourceStoreError::AdmissionUnavailable)?;
            }
            length
        }
    };
    let mut metadata = std::mem::size_of::<LoadedSource>()
        + source.id.as_str().len()
        + source.path.as_os_str().len();
    if let SourceRevision::Disk {
        read_policy,
        path_entry,
        ..
    } = &source.revision
    {
        read_policy
            .visit_owned_clone_payload(&mut |n| {
                metadata = metadata.checked_add(n).ok_or("metadata overflow")?;
                Ok(())
            })
            .map_err(|_| SourceStoreError::AdmissionUnavailable)?;
        path_entry
            .visit_recovery_payload(&mut |n| {
                metadata = metadata.checked_add(n).ok_or("metadata overflow")?;
                Ok(())
            })
            .map_err(|_| SourceStoreError::AdmissionUnavailable)?;
    }
    // UTF-8 copies directly to Arc<str>. Other encodings need a separately
    // owned temporary String; reserve only that encoding's exact output size.
    let _scratch = if matches!(encoding, Encoding::Utf8(_)) {
        None
    } else {
        Some(
            budget
                .admit_owned(1, 0, decoded_bytes, || ())
                .map_err(encoding_admission_error)?,
        )
    };
    let payload = metadata
        .checked_add(decoded_bytes)
        .and_then(|n| n.checked_add(2 * std::mem::size_of::<usize>()))
        .ok_or(SourceStoreError::AdmissionUnavailable)?;
    LoadedSource::admit_construction(budget, decoded_bytes, payload, || {
        poll()?;
        let text = match encoding {
            Encoding::Utf8(text) => Arc::<str>::from(text),
            Encoding::Utf16(bytes, little) => {
                let mut text = String::with_capacity(decoded_bytes);
                for (n, ch) in decoded_utf16_chars(bytes, little).enumerate() {
                    if n % 4096 == 0 {
                        poll()?;
                    }
                    text.push(ch);
                }
                Arc::<str>::from(text)
            }
            Encoding::Latin1(bytes) => {
                let mut text = String::with_capacity(decoded_bytes);
                for (n, byte) in bytes.iter().enumerate() {
                    if n % 4096 == 0 {
                        poll()?;
                    }
                    text.push(*byte as char);
                }
                Arc::<str>::from(text)
            }
        };
        poll()?;
        let mut decoded = source.clone();
        decoded.decoded_text = Some(text);
        Ok(decoded)
    })
}

impl LspSourceStore {
    pub(crate) fn from_input(context: Arc<ProjectContext>, input: &WorkspaceInput) -> Self {
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
            project_cache: input.project_cache.clone(),
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

    pub(super) fn with_directory_limit(mut self, limit: usize) -> Self {
        self.disk =
            FilesystemSourceStore::with_limits(limit, ResolverLimits::default().max_scanned_bytes);
        self
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
        let read_policy = self.context.read_policy().clone();
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
    fn list_directory_admitted(
        &mut self,
        request: DirectoryRequest<'_>,
        budget: &mut pascal_core::compile_time::CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<pascal_core::compile_time::AdmittedOwned<DirectoryListing>, SourceStoreError> {
        let listing = self.disk.list_directory_admitted(request, budget, cancel)?;
        let overlays = || {
            self.overlays.keys().filter(|path| {
                path.parent()
                    .is_some_and(|p| path_equivalent(p, request.directory))
                    && self.context.path_entry_for(path).is_some()
            })
        };
        let count = listing.files.len() + overlays().count();
        let bytes = listing
            .files
            .iter()
            .chain(listing.directories.iter())
            .chain(overlays())
            .try_fold(
                std::mem::size_of::<DirectoryListing>()
                    + (count + listing.directories.len()) * std::mem::size_of::<PathBuf>(),
                |n, p| n.checked_add(p.as_os_str().len()),
            )
            .ok_or(SourceStoreError::AdmissionUnavailable)?;
        budget
            .admit_owned(count, bytes, bytes, || {
                let mut files = Vec::with_capacity(count);
                files.extend(listing.files.iter().cloned());
                for path in overlays() {
                    if !files.iter().any(|p| path_equivalent(p, path)) {
                        files.push(path.clone());
                    }
                }
                DirectoryListing {
                    files,
                    directories: listing.directories.clone(),
                    stamp: listing.stamp.clone(),
                    complete: listing.complete,
                }
            })
            .map_err(|_| SourceStoreError::AdmissionUnavailable)
    }

    fn overlay_candidates_admitted(
        &self,
        roots: &[PathBuf],
        names: &[String],
        budget: &mut pascal_core::compile_time::CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<pascal_core::compile_time::AdmittedOwned<Vec<PathBuf>>, SourceStoreError> {
        if cancel.is_cancelled() {
            return Err(SourceStoreError::Cancelled);
        }
        let bytes = self
            .overlays
            .keys()
            .try_fold(0usize, |n, p| {
                n.checked_add(p.as_os_str().len() + std::mem::size_of::<PathBuf>())
            })
            .ok_or(SourceStoreError::AdmissionUnavailable)?;
        budget
            .admit_owned(self.overlays.len(), bytes, bytes, || {
                self.overlay_candidates(roots, names)
            })
            .map_err(|_| SourceStoreError::AdmissionUnavailable)
    }

    fn load_admitted(
        &mut self,
        request: SourceRequest<'_>,
        budget: &mut pascal_core::compile_time::CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<LoadedSource, SourceStoreError> {
        if cancel.is_cancelled() {
            return Err(SourceStoreError::Cancelled);
        }
        let legacy = matches!(
            request.entry.provenance,
            ProjectPathProvenance::LegacyNative
        ) && request.read_policy.allows_legacy_route_entry(request.entry);
        if !path_equivalent(request.path, &request.entry.path)
            || (!legacy && !request.read_policy.allows_location(request.entry))
        {
            return Err(SourceStoreError::AdmissionUnavailable);
        }
        if !self.overlays.contains_key(request.path) {
            // Authenticate and admit raw backing before reading. Only after
            // inspecting those exact bytes admit the selected encoding's
            // coordinate backing, without simultaneous worst-case decoders.
            #[cfg(test)]
            record_test_source_load(request.path);
            let raw = self.disk.load_raw_admitted(request, budget, cancel)?;
            return admit_decoded_source(raw, budget, cancel);
        }
        let size = if let Some(overlay) = self.overlays.get(request.path) {
            overlay.text.len()
        } else {
            if !(request.read_policy.allows_entry(request.entry)
                || legacy
                    && request.legacy_route.is_some()
                    && request
                        .read_policy
                        .allows_legacy_payload_entry(request.entry))
            {
                return self.disk.load_admitted(request, budget, cancel);
            }
            let metadata = std::fs::metadata(request.path).map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    SourceStoreError::NotFound {
                        path: request.path.into(),
                    }
                } else {
                    SourceStoreError::Io {
                        path: request.path.into(),
                        message: e.to_string(),
                    }
                }
            })?;
            usize::try_from(metadata.len()).map_err(|_| SourceStoreError::AdmissionUnavailable)?
        };
        if size > request.max_bytes {
            return Err(SourceStoreError::TooLarge {
                path: request.path.into(),
                maximum: request.max_bytes,
            });
        }
        // Bound the raw read, worst-case decoded UTF-8 backing and transient
        // Arc/String copies *before* invoking the authorized bounded loader.
        let mut metadata = std::mem::size_of::<LoadedSource>()
            + "source:".len()
            + 4 * request.path.as_os_str().len();
        request
            .read_policy
            .visit_owned_clone_payload(&mut |n| {
                metadata = metadata.checked_add(n).ok_or("metadata overflow")?;
                Ok(())
            })
            .map_err(|_| SourceStoreError::AdmissionUnavailable)?;
        request
            .entry
            .visit_recovery_payload(&mut |n| {
                metadata = metadata.checked_add(n).ok_or("metadata overflow")?;
                Ok(())
            })
            .map_err(|_| SourceStoreError::AdmissionUnavailable)?;
        let payload = size
            .checked_mul(8)
            .and_then(|n| n.checked_add(metadata))
            .ok_or(SourceStoreError::AdmissionUnavailable)?;
        LoadedSource::admit_construction(budget, size, payload, || {
            self.load(
                SourceRequest {
                    max_bytes: size,
                    ..request
                },
                cancel,
            )
        })
    }

    fn declared_unit_name(&self, source: &LoadedSource) -> Option<Option<String>> {
        let uri = Url::from_file_path(&source.path).ok()?;
        let hash = match &source.revision {
            SourceRevision::Disk { content_hash, .. }
            | SourceRevision::Overlay { content_hash, .. } => *content_hash,
        };
        self.project_cache.declared_unit_name(&uri, hash)
    }

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
                bytes: Arc::<[u8]>::from(overlay.text.clone()),
                decoded_text: Some(overlay.text.clone()),
                compile_time_backing: None,
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

        #[cfg(test)]
        record_test_disk_read(&path);
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
    context: Arc<ProjectContext>,
    observations: &ProjectObservations,
    roots: Vec<PathBuf>,
    input: &WorkspaceInput,
    _cancel: &AtomicBool,
) -> UnitResolver<LspSourceStore> {
    let mut limits = ResolverLimits::default();
    limits.max_dependency_units = MAX_DEPENDENCY_WORK;
    limits.max_source_bytes = input.options.limits.max_file_bytes;
    limits.max_compile_time_retained_bytes = super::compile_time::lsp_compatibility_retention(
        input.options.limits,
        crate::include_expansion::ExpansionLimits::default(),
    );
    resolver_for_context_with_limits(context, observations, roots, input, limits)
}

pub(crate) fn resolver_for_context_with_limits(
    context: Arc<ProjectContext>,
    observations: &ProjectObservations,
    roots: Vec<PathBuf>,
    input: &WorkspaceInput,
    mut limits: ResolverLimits,
) -> UnitResolver<LspSourceStore> {
    limits.max_compile_time_byte_work = byte_work_cap(limits.max_compile_time_byte_work);
    UnitResolver::new(
        context.clone(),
        roots,
        LspSourceStore::from_input(context, input),
        limits,
    )
    .with_context_observations(observations)
}

pub(crate) fn resolver_for_context_with_session_cache(
    context: Arc<ProjectContext>,
    observations: &ProjectObservations,
    roots: Vec<PathBuf>,
    input: &WorkspaceInput,
    session_cache: ResolverSessionCache,
) -> UnitResolver<LspSourceStore> {
    let mut limits = ResolverLimits::default();
    // Candidate selection proves prefixes, not provider bodies. Body sessions
    // still apply CompileTimeLimits::default() and the separate 64-state fuse.
    limits.max_compile_time_byte_work = pascal_core::compile_time::HEADER_MAX_BYTE_WORK;
    limits.max_dependency_units = MAX_DEPENDENCY_WORK;
    limits.max_source_bytes = input.options.limits.max_file_bytes;
    limits.max_compile_time_retained_bytes = super::compile_time::lsp_compatibility_retention(
        input.options.limits,
        crate::include_expansion::ExpansionLimits::default(),
    );
    resolver_for_context_with_limits_and_session_cache(
        context,
        observations,
        roots,
        limits,
        input,
        session_cache,
    )
}

fn resolver_for_context_with_limits_and_session_cache(
    context: Arc<ProjectContext>,
    observations: &ProjectObservations,
    roots: Vec<PathBuf>,
    mut limits: ResolverLimits,
    input: &WorkspaceInput,
    session_cache: ResolverSessionCache,
) -> UnitResolver<LspSourceStore> {
    limits.max_compile_time_byte_work = byte_work_cap(limits.max_compile_time_byte_work);
    UnitResolver::new_with_session_cache(
        context.clone(),
        roots,
        LspSourceStore::from_input(context, input),
        limits,
        session_cache,
    )
    .with_context_observations(observations)
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
    let parsed_text_hash = super::rename::text_content_hash(&text);
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
            parsed_text_hash: Some(parsed_text_hash),
            content_bytes: None,
            candidate_membership: None,
            candidate_observations: Vec::new(),
            read_policy: Some(context.read_policy().clone()),
            path_entry: context.path_entry_for(&source.path),
            include_payload,
            missing_provider_candidate: false,
            document_link_missing_candidate: false,
            document_link_case_variant: None,
            directory_observation: false,
            document_link_ancestor: false,
            missing_provider_scope: None,
            auto_import_provider_observation: false,
            auto_import_scopes: Vec::new(),
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
            parsed_text_hash: Some(parsed_text_hash),
            content_bytes: None,
            candidate_membership: None,
            candidate_observations: Vec::new(),
            read_policy: Some(read_policy.clone()),
            path_entry: Some(path_entry.clone()),
            include_payload,
            missing_provider_candidate: false,
            document_link_missing_candidate: false,
            document_link_case_variant: None,
            directory_observation: false,
            document_link_ancestor: false,
            missing_provider_scope: None,
            auto_import_provider_observation: false,
            auto_import_scopes: Vec::new(),
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
            parsed_text_hash: None,
            content_bytes: None,
            candidate_membership: None,
            candidate_observations: Vec::new(),
            read_policy: Some(read_policy.clone()),
            path_entry: Some(path_entry.clone()),
            include_payload: false,
            missing_provider_candidate: false,
            document_link_missing_candidate: false,
            document_link_case_variant: None,
            directory_observation: false,
            document_link_ancestor: false,
            missing_provider_scope: None,
            auto_import_provider_observation: false,
            auto_import_scopes: Vec::new(),
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
            parsed_text_hash: None,
            content_bytes: None,
            candidate_membership: None,
            candidate_observations: Vec::new(),
            read_policy: Some(context.read_policy().clone()),
            path_entry: context.path_entry_for(path),
            include_payload: false,
            missing_provider_candidate: false,
            document_link_missing_candidate: false,
            document_link_case_variant: None,
            directory_observation: false,
            document_link_ancestor: false,
            missing_provider_scope: None,
            auto_import_provider_observation: false,
            auto_import_scopes: Vec::new(),
        }),
    }
}

pub(crate) fn observation_record(observation: &ResolutionObservation) -> Option<SourceRecord> {
    let (path, stamp, missing_provider_candidate, directory_observation, candidate_observation) =
        match observation {
            ResolutionObservation::Directory {
                path,
                stamp,
                complete: _,
                entry: _,
            } => (path, stamp.clone(), false, true, None),
            ResolutionObservation::Candidate {
                path,
                stamp,
                present,
                entry: _,
            } => (
                path,
                stamp.clone(),
                !present,
                false,
                Some(ResolverCandidateObservation {
                    path: path.clone(),
                    present: *present,
                }),
            ),
            ResolutionObservation::Metadata(_) | ResolutionObservation::ProjectRead(_) => {
                return None;
            }
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
        parsed_text_hash: None,
        content_bytes: None,
        candidate_membership: None,
        candidate_observations: candidate_observation.into_iter().collect(),
        read_policy: None,
        path_entry: None,
        include_payload: false,
        missing_provider_candidate,
        document_link_missing_candidate: false,
        document_link_case_variant: None,
        directory_observation,
        document_link_ancestor: false,
        missing_provider_scope: None,
        auto_import_provider_observation: false,
        auto_import_scopes: Vec::new(),
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
                        Some(context.read_policy().clone()),
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
            parsed_text_hash: None,
            content_bytes,
            candidate_membership: None,
            candidate_observations: Vec::new(),
            read_policy,
            path_entry,
            include_payload: false,
            missing_provider_candidate: false,
            document_link_missing_candidate: false,
            document_link_case_variant: None,
            directory_observation: false,
            document_link_ancestor: false,
            missing_provider_scope: None,
            auto_import_provider_observation: false,
            auto_import_scopes: Vec::new(),
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
        let candidate_observations = std::mem::take(&mut existing.candidate_observations);
        *existing = incoming;
        merge_candidate_observations(&mut existing.candidate_observations, candidate_observations);
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
    merge_candidate_observations(
        &mut existing.candidate_observations,
        incoming.candidate_observations,
    );
    if existing.read_policy.is_none() {
        existing.read_policy = incoming.read_policy.clone();
    }
    if existing.path_entry.is_none() {
        existing.path_entry = incoming.path_entry.clone();
    }
    existing.open |= incoming.open;
    existing.include_payload |= incoming.include_payload;
    existing.missing_provider_candidate |= incoming.missing_provider_candidate;
    // Two witnesses for one spelling must agree on the accepted case variant.
    if incoming.document_link_missing_candidate {
        existing.document_link_case_variant = if existing.document_link_missing_candidate
            && existing.document_link_case_variant != incoming.document_link_case_variant
        {
            None
        } else {
            incoming.document_link_case_variant.clone()
        };
    }
    existing.document_link_missing_candidate |= incoming.document_link_missing_candidate;
    existing.directory_observation |= incoming.directory_observation;
    // Any other observation of the same directory keeps the strict check.
    existing.document_link_ancestor &= incoming.document_link_ancestor;
}

fn merge_candidate_observations(
    target: &mut Vec<ResolverCandidateObservation>,
    incoming: Vec<ResolverCandidateObservation>,
) {
    for observation in incoming {
        if target.iter().any(|existing| {
            existing
                .path
                .to_string_lossy()
                .eq_ignore_ascii_case(&observation.path.to_string_lossy())
        }) {
            continue;
        }
        if target.len() >= super::rename::MAX_RESOLVER_CANDIDATE_OBSERVATIONS {
            break;
        }
        target.push(observation);
    }
}

pub(crate) fn merge_report_into_context(
    observations: &mut ProjectObservations,
    report: &ResolutionReport,
) {
    for observation in &report.observations {
        match observation {
            pascal_core::ResolutionObservation::Metadata(observation) => {
                pascal_project::add_metadata_observation(
                    &mut observations.metadata_observations,
                    observation.clone(),
                );
                if !observations
                    .metadata_files
                    .iter()
                    .any(|path| path_equivalent(path, observation.path()))
                {
                    observations
                        .metadata_files
                        .push(observation.path().to_path_buf());
                }
            }
            pascal_core::ResolutionObservation::ProjectRead(observation) => {
                if !observations
                    .metadata_files
                    .iter()
                    .any(|path| path_equivalent(path, &observation.path))
                {
                    observations.metadata_files.push(observation.path.clone());
                }
            }
            pascal_core::ResolutionObservation::Directory { .. }
            | pascal_core::ResolutionObservation::Candidate { .. }
            | pascal_core::ResolutionObservation::Payload { .. } => {}
        }
    }
    for warning in &report.warnings {
        if !observations
            .warnings
            .iter()
            .any(|existing| existing == warning)
        {
            observations.warnings.push(warning.clone());
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
    pascal_project::path_identity::path_key(&absolute_path(path.to_path_buf())).replace('\\', "/")
}

fn path_equivalent(left: &Path, right: &Path) -> bool {
    path_key(left) == path_key(right)
}

fn path_starts_with(path: &Path, root: &Path) -> bool {
    pascal_project::path_identity::path_starts_with(
        &absolute_path(path.to_path_buf()),
        &absolute_path(root.to_path_buf()),
    )
}

#[cfg(test)]
mod admitted_encoding_tests {
    use super::*;
    use pascal_core::NoCancellation;
    use pascal_core::compile_time::{CompileTimeBudget, CompileTimeLimits};
    use pascal_project::{ProjectOptions, delphi_overrides::OverrideSession};
    use std::cell::Cell;

    fn load(
        bytes: &[u8],
        limit: usize,
        cancel: &dyn CancellationToken,
    ) -> Result<LoadedSource, SourceStoreError> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Source.inc");
        std::fs::write(&path, bytes).unwrap();
        let workspace = super::super::Workspace::with_override_session(
            vec![dir.path().into()],
            super::super::WorkspaceOptions::default(),
            OverrideSession::new(None),
        );
        let context =
            ProjectContext::discover(&path, &[dir.path().into()], &ProjectOptions::default())
                .unwrap()
                .context;
        let entry = context.path_entry_for(&path).unwrap();
        let policy = context.read_policy().clone();
        let mut store = LspSourceStore::from_input(context, &workspace.analysis_input());
        let mut budget = CompileTimeBudget::new(
            CompileTimeLimits {
                max_retained_bytes: limit,
                ..CompileTimeLimits::default()
            },
            cancel,
        );
        let source = store.load_admitted(
            SourceRequest {
                path: &path,
                entry: &entry,
                read_policy: &policy,
                legacy_route: None,
                kind: SourceKind::Include,
                max_bytes: bytes.len(),
            },
            &mut budget,
            cancel,
        )?;
        assert_eq!(
            source.bytes.as_ref(),
            bytes,
            "raw revision payload cannot become decoded fiction"
        );
        assert!(source.compile_time_backing.is_some());
        assert!(budget.snapshot().retained_bytes <= limit);
        Ok(source)
    }

    #[test]
    fn utf8_child_admission_does_not_reserve_other_encoding_scratch() {
        let text = format!("{{é😀{}}}\nconst Value = 1;", "x".repeat(208_000));
        let source = load(text.as_bytes(), 1_048_576, &NoCancellation).unwrap();
        assert_eq!(source.decoded_text.as_deref(), Some(text.as_str()));
        assert!(
            matches!(source.revision, SourceRevision::Disk { content_hash, .. } if content_hash == content_hash_bytes(text.as_bytes()))
        );
    }

    #[test]
    fn utf16_and_fallback_admission_preserve_decoded_coordinates_and_raw_hash() {
        let text = "{é😀}\r\nunit Boundary;";
        for little in [true, false] {
            let mut bytes = if little {
                vec![0xff, 0xfe]
            } else {
                vec![0xfe, 0xff]
            };
            for unit in text.encode_utf16() {
                bytes.extend(if little {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                });
            }
            let source = load(&bytes, 65_536, &NoCancellation).unwrap();
            assert_eq!(source.decoded_text.as_deref(), Some(text));
            assert!(
                matches!(source.revision, SourceRevision::Disk { content_hash, .. } if content_hash == content_hash_bytes(&bytes))
            );
            assert_eq!(
                source.decoded_text.as_ref().unwrap().find("Boundary"),
                Some(text.find("Boundary").unwrap())
            );
        }
        let bytes = b"{caf\xe9}\nunit Boundary;";
        let source = load(bytes, 65_536, &NoCancellation).unwrap();
        assert_eq!(
            source.decoded_text.as_deref(),
            Some("{café}\nunit Boundary;")
        );
    }

    #[test]
    fn invalid_encoded_units_keep_existing_lossy_and_fallback_contract() {
        for bytes in [
            vec![0xff, 0xfe, 0x00, 0xd8, b'A', 0, 0xff],
            vec![0xc3, 0x28, 0xff],
        ] {
            let expected = decode_source_bytes(&bytes);
            let source = load(&bytes, 65_536, &NoCancellation).unwrap();
            assert_eq!(source.decoded_text.as_deref(), Some(expected.as_str()));
        }
    }

    struct PollCancel {
        polls: Cell<usize>,
        stop: usize,
    }
    impl CancellationToken for PollCancel {
        fn is_cancelled(&self) -> bool {
            let n = self.polls.get() + 1;
            self.polls.set(n);
            n >= self.stop
        }
    }

    #[test]
    fn encoding_admission_cancellation_never_publishes_partial_coordinate_text() {
        let bytes = [0xff, 0xfe, b'A', 0, 0x00, 0xd8];
        let count = PollCancel {
            polls: Cell::new(0),
            stop: usize::MAX,
        };
        load(&bytes, 65_536, &count).unwrap();
        for stop in 1..=count.polls.get() {
            let cancel = PollCancel {
                polls: Cell::new(0),
                stop,
            };
            assert!(
                matches!(
                    load(&bytes, 65_536, &cancel),
                    Err(SourceStoreError::Cancelled)
                ),
                "poll {stop}"
            );
        }
    }
}

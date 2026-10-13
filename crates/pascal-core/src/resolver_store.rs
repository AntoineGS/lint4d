//! Filesystem and overlay source-store implementation for the shared resolver.

use super::{
    CancellationToken, DirectoryListing, DirectoryRequest, LoadedSource, ProjectPathEntry,
    ProjectPathProvenance, ResolverLimits, SourceRequest, SourceRevision, SourceStore,
    SourceStoreError, canonical_path, content_hash_bytes, path_equivalent, path_key,
    path_starts_with, source_id_for_path,
};
use pascal_project::{path_identity, path_stamp_result};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// An in-memory overlay usable by [`FilesystemSourceStore`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlaySource {
    pub version: i32,
    pub bytes: Arc<[u8]>,
}

impl OverlaySource {
    pub fn new(version: i32, bytes: impl Into<Arc<[u8]>>) -> Self {
        Self {
            version,
            bytes: bytes.into(),
        }
    }
}

/// Disk source provider with an explicit, request-scoped overlay table.
#[derive(Debug, Clone)]
pub struct FilesystemSourceStore {
    overlays: HashMap<PathBuf, OverlaySource>,
    max_directory_entries: usize,
    max_scanned_bytes: usize,
    scanned_bytes: usize,
}

impl Default for FilesystemSourceStore {
    fn default() -> Self {
        Self::new()
    }
}

impl FilesystemSourceStore {
    /// Bounded raw-byte capability for adapters whose coordinate decoder runs
    /// in the resolver. Admission occurs before the physical payload read.
    pub fn load_raw_admitted(
        &mut self,
        request: SourceRequest<'_>,
        budget: &mut crate::compile_time::CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<LoadedSource, SourceStoreError> {
        self.load_with_admission(request, budget, cancel, false)
    }

    fn load_with_admission(
        &mut self,
        request: SourceRequest<'_>,
        budget: &mut crate::compile_time::CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
        decode: bool,
    ) -> Result<LoadedSource, SourceStoreError> {
        use super::store_admission_error;
        if cancel.is_cancelled() {
            return Err(SourceStoreError::Cancelled);
        }
        // Borrowed authorization precedes even metadata IO. The existing loader
        // repeats route/symlink/opened-file authorization at the read boundary.
        let legacy = matches!(
            request.entry.provenance,
            ProjectPathProvenance::LegacyNative
        ) && request.read_policy.allows_legacy_route_entry(request.entry);
        if !path_equivalent(request.path, &request.entry.path)
            || (!legacy && !request.read_policy.allows_location(request.entry))
        {
            return Err(SourceStoreError::AdmissionUnavailable);
        }
        let mut metadata = 4 * request.path.as_os_str().len() + std::mem::size_of::<LoadedSource>();
        request
            .read_policy
            .visit_owned_clone_payload(&mut |n| {
                metadata = metadata.checked_add(n).ok_or("source metadata overflow")?;
                Ok(())
            })
            .map_err(|_| SourceStoreError::AdmissionUnavailable)?;
        request
            .entry
            .visit_recovery_payload(&mut |n| {
                metadata = metadata.checked_add(n).ok_or("source metadata overflow")?;
                Ok(())
            })
            .map_err(|_| SourceStoreError::AdmissionUnavailable)?;
        let _metadata_scratch = budget
            .admit_owned(1, metadata, metadata, || ())
            .map_err(|r| store_admission_error(r, budget))?;
        let overlay = self.overlays.get(request.path);
        let legacy_payload = legacy
            && request.legacy_route.is_some()
            && request
                .read_policy
                .allows_legacy_payload_entry(request.entry);
        if overlay.is_none() && !legacy_payload && !request.read_policy.allows_entry(request.entry)
        {
            // Location authorization above permits a bounded spelling lookup,
            // not a payload read. A genuinely absent entry can take the existing
            // NotFound case-match route; existing denied entries (including
            // dangling symlinks) must never acquire that route. The matched
            // physical entry is independently authorized by a subsequent load.
            if fs::symlink_metadata(request.path)
                .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
            {
                return Err(SourceStoreError::NotFound {
                    path: request.path.to_path_buf(),
                });
            }
            return Err(SourceStoreError::Unauthorized {
                path: request.path.into(),
                reason: "payload is not authorized".into(),
            });
        }
        let size = if let Some((_, overlay)) = self
            .overlays
            .iter()
            .find(|(path, _)| path_equivalent(path, request.path))
        {
            overlay.bytes.len()
        } else {
            let stat = fs::metadata(request.path).map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    SourceStoreError::NotFound {
                        path: request.path.to_path_buf(),
                    }
                } else {
                    SourceStoreError::Io {
                        path: request.path.to_path_buf(),
                        message: error.to_string(),
                    }
                }
            })?;
            usize::try_from(stat.len()).map_err(|_| SourceStoreError::AdmissionUnavailable)?
        };
        let payload = size
            .checked_mul(if decode { 4 } else { 1 })
            .and_then(|n| n.checked_add(metadata))
            .ok_or(SourceStoreError::AdmissionUnavailable)?;
        // read_to_end may grow up to twice the bounded input; decode temporarily
        // owns a worst-case 3-byte-per-input UTF8 String before Arc conversion.
        let scratch = size
            .checked_mul(if decode { 5 } else { 2 })
            .and_then(|n| n.checked_add(64))
            .ok_or(SourceStoreError::AdmissionUnavailable)?;
        let _read_scratch = budget
            .admit_owned(1, size, scratch, || ())
            .map_err(|r| store_admission_error(r, budget))?;
        LoadedSource::admit_construction(budget, size, payload, || {
            self.load_inner(
                SourceRequest {
                    max_bytes: request.max_bytes.min(size),
                    ..request
                },
                cancel,
                decode,
            )
        })
    }

    fn push_admitted_path(
        listing: &mut crate::compile_time::AdmittedOwned<DirectoryListing>,
        path_bytes: usize,
        build: impl FnOnce() -> PathBuf,
        directory: bool,
        budget: &mut crate::compile_time::CompileTimeBudget<'_>,
    ) -> Result<(), SourceStoreError> {
        let v = if directory {
            &listing.directories
        } else {
            &listing.files
        };
        let old = v.capacity();
        let growth = v.len() == old;
        let capacity = if growth {
            (old + 1) * std::mem::size_of::<PathBuf>()
        } else {
            0
        };
        let unused_path_reservation = listing
            .grow(budget, 1, path_bytes, path_bytes + capacity, |listing| {
                let v = if directory {
                    &mut listing.directories
                } else {
                    &mut listing.files
                };
                if growth {
                    v.reserve_exact(1);
                }
                let path = build();
                let unused = path_bytes.saturating_sub(path.capacity());
                v.push(path);
                unused
            })
            .map_err(|r| super::store_admission_error(r, budget))?;
        // Keep the pre-allocation OS filename bound until construction ends,
        // then retire only its unused portion, not the live PathBuf capacity.
        listing
            .retire_storage(unused_path_reservation)
            .map_err(|r| super::store_admission_error(r, budget))?;
        if growth {
            listing
                .retire_storage(old * std::mem::size_of::<PathBuf>())
                .map_err(|r| super::store_admission_error(r, budget))?;
        }
        Ok(())
    }
    pub fn new() -> Self {
        Self {
            overlays: HashMap::new(),
            max_directory_entries: ResolverLimits::default().max_directory_entries,
            max_scanned_bytes: ResolverLimits::default().max_scanned_bytes,
            scanned_bytes: 0,
        }
    }

    pub fn with_limits(max_directory_entries: usize, max_scanned_bytes: usize) -> Self {
        Self {
            max_directory_entries,
            max_scanned_bytes,
            ..Self::new()
        }
    }

    pub fn insert_overlay(
        &mut self,
        path: impl AsRef<Path>,
        version: i32,
        bytes: impl Into<Arc<[u8]>>,
    ) {
        self.overlays.insert(
            canonical_path(path.as_ref()),
            OverlaySource::new(version, bytes),
        );
    }

    pub fn overlays(&self) -> &HashMap<PathBuf, OverlaySource> {
        &self.overlays
    }

    fn overlay_for(&self, path: &Path) -> Option<(PathBuf, OverlaySource)> {
        let path = canonical_path(path);
        if let Some(overlay) = self.overlays.get(&path) {
            return Some((path, overlay.clone()));
        }
        // A client may spell a path differently from the disk, e.g. with a
        // lowercase drive letter; on a case-insensitive volume it is the same
        // file. A file on disk keeps the on-disk spelling `load` found; an
        // unsaved one has only the overlay's spelling.
        if !path_identity::is_case_insensitive(&path) {
            return None;
        }
        self.overlays
            .iter()
            .find(|(overlay_path, _)| path_equivalent(overlay_path, &path))
            .map(|(overlay_path, overlay)| {
                let identity = if fs::symlink_metadata(&path).is_ok() {
                    path
                } else {
                    overlay_path.clone()
                };
                (identity, overlay.clone())
            })
    }
}

impl SourceStore for FilesystemSourceStore {
    fn load_admitted(
        &mut self,
        request: SourceRequest<'_>,
        budget: &mut crate::compile_time::CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<LoadedSource, SourceStoreError> {
        self.load_with_admission(request, budget, cancel, true)
    }

    fn overlay_candidates_admitted(
        &self,
        roots: &[PathBuf],
        names: &[String],
        budget: &mut crate::compile_time::CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<crate::compile_time::AdmittedOwned<Vec<PathBuf>>, SourceStoreError> {
        let mut out = budget
            .admit_owned(0, 0, 0, Vec::new)
            .map_err(|r| super::store_admission_error(r, budget))?;
        for path in self.overlays.keys() {
            if cancel.is_cancelled() {
                return Err(SourceStoreError::Cancelled);
            }
            let bytes = path.as_os_str().len();
            let _matching_scratch = budget
                .admit_owned(1, bytes, bytes.saturating_mul(3), || ())
                .map_err(|r| super::store_admission_error(r, budget))?;
            if roots.iter().any(|root| path_starts_with(path, root))
                && path.file_name().is_some_and(|file| {
                    names
                        .iter()
                        .any(|name| file.to_string_lossy().eq_ignore_ascii_case(name))
                })
            {
                let old = out.capacity();
                let growth = out.len() == old;
                let bytes = path.as_os_str().len();
                out.grow(
                    budget,
                    1,
                    bytes,
                    bytes
                        + if growth {
                            (old + 1) * std::mem::size_of::<PathBuf>()
                        } else {
                            0
                        },
                    |v| {
                        if growth {
                            v.reserve_exact(1);
                        }
                        v.push(path.clone());
                    },
                )
                .map_err(|r| super::store_admission_error(r, budget))?;
                if growth {
                    out.retire_storage(old * std::mem::size_of::<PathBuf>())
                        .map_err(|r| super::store_admission_error(r, budget))?;
                }
            }
        }
        Ok(out)
    }

    fn list_directory_admitted(
        &mut self,
        request: DirectoryRequest<'_>,
        budget: &mut crate::compile_time::CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<crate::compile_time::AdmittedOwned<DirectoryListing>, SourceStoreError> {
        if cancel.is_cancelled() {
            return Err(SourceStoreError::Cancelled);
        }
        let legacy = matches!(
            request.entry.provenance,
            ProjectPathProvenance::LegacyNative
        ) && request.read_policy.allows_legacy_route_entry(request.entry);
        if !path_equivalent(request.directory, &request.entry.path)
            || (!legacy && !request.read_policy.allows_location(request.entry))
        {
            return Err(SourceStoreError::AdmissionUnavailable);
        }
        let mut listing = budget
            .admit_owned(1, 0, 0, || DirectoryListing {
                files: vec![],
                directories: vec![],
                stamp: path_stamp_result(request.directory).ok().flatten(),
                complete: true,
            })
            .map_err(|r| super::store_admission_error(r, budget))?;
        if fs::symlink_metadata(request.directory).is_ok_and(|m| m.file_type().is_symlink())
            && !legacy
        {
            listing.value_mut().complete = false;
            return Ok(listing);
        }
        // Overlay lengths are available by borrow. Admit BEFORE cloning even an
        // unrelated path; directory observations contain all overlay entries.
        for path in self.overlays.keys().filter(|p| {
            p.parent()
                .is_some_and(|p| path_equivalent(p, request.directory))
        }) {
            if cancel.is_cancelled() {
                return Err(SourceStoreError::Cancelled);
            }
            Self::push_admitted_path(
                &mut listing,
                path.as_os_str().len(),
                || path.clone(),
                false,
                budget,
            )?;
        }
        let entries = match fs::read_dir(request.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound && !listing.files.is_empty() => {
                return Ok(listing);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(SourceStoreError::NotFound {
                    path: request.directory.into(),
                });
            }
            Err(_) => {
                listing.value_mut().complete = false;
                return Ok(listing);
            }
        };
        for (n, entry) in entries.enumerate() {
            if cancel.is_cancelled() {
                return Err(SourceStoreError::Cancelled);
            }
            if n >= self.max_directory_entries {
                listing.value_mut().complete = false;
                break;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    listing.value_mut().complete = false;
                    continue;
                }
            };
            let kind = match entry.file_type() {
                Ok(k) => k,
                Err(_) => {
                    listing.value_mut().complete = false;
                    continue;
                }
            };
            // The OS filename bound is known before DirEntry::path allocates.
            let filename_bound = if cfg!(windows) { 1024 } else { 255 };
            let bound = request.directory.as_os_str().len() + 1 + filename_bound;
            // A symlink's spelling is not permission to follow its target.
            // Match the ordinary name-only listing; payload authorization is
            // still checked independently when a selected candidate is read.
            if kind.is_file() || kind.is_dir() || kind.is_symlink() {
                Self::push_admitted_path(
                    &mut listing,
                    bound,
                    || entry.path(),
                    kind.is_dir(),
                    budget,
                )?;
            }
        }
        Ok(listing)
    }
    fn list_directory(
        &mut self,
        request: DirectoryRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<DirectoryListing, SourceStoreError> {
        if cancel.is_cancelled() {
            return Err(SourceStoreError::Cancelled);
        }
        let directory = canonical_path(request.directory);
        if !path_equivalent(&directory, &request.entry.path) {
            return Err(SourceStoreError::Unauthorized {
                path: directory,
                reason: "directory and authorization entry identify different paths".to_string(),
            });
        }
        let entry = ProjectPathEntry {
            path: directory.clone(),
            provenance: request.entry.provenance.clone(),
        };
        let legacy_location = matches!(entry.provenance, ProjectPathProvenance::LegacyNative)
            && request.read_policy.allows_legacy_route_entry(&entry);
        if !legacy_location && !request.read_policy.allows_location(&entry) {
            return Err(SourceStoreError::Unauthorized {
                path: directory,
                reason: "directory is outside the requester-scoped read policy".to_string(),
            });
        }
        let stamp = path_stamp_result(&directory).ok().flatten();
        let mut files = Vec::new();
        let mut directories = Vec::new();
        let mut complete = true;
        if let Ok(metadata) = fs::symlink_metadata(&directory) {
            if metadata.file_type().is_symlink() {
                let legacy_directory =
                    matches!(entry.provenance, ProjectPathProvenance::LegacyNative)
                        && request.read_policy.allows_legacy_route_entry(&entry);
                if !legacy_directory {
                    return Ok(DirectoryListing {
                        files,
                        directories,
                        stamp,
                        complete: false,
                    });
                }
            }
            if !metadata.is_dir() && !metadata.file_type().is_symlink() {
                return Ok(DirectoryListing {
                    files,
                    directories,
                    stamp,
                    complete: false,
                });
            }
        }
        let mut visited = 0usize;
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let overlay_files = self
                    .overlays
                    .keys()
                    .filter(|path| path.parent().is_some_and(|parent| parent == directory))
                    .cloned()
                    .collect::<Vec<_>>();
                if overlay_files.is_empty() {
                    return Err(SourceStoreError::NotFound { path: directory });
                }
                return Ok(DirectoryListing {
                    files: overlay_files,
                    directories: Vec::new(),
                    stamp,
                    complete: true,
                });
            }
            Err(_) => {
                return Ok(DirectoryListing {
                    files,
                    directories,
                    stamp,
                    complete: false,
                });
            }
        };
        for entry in entries {
            if cancel.is_cancelled() {
                return Err(SourceStoreError::Cancelled);
            }
            visited = visited.saturating_add(1);
            if visited > self.max_directory_entries {
                complete = false;
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    complete = false;
                    continue;
                }
            };
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => {
                    complete = false;
                    continue;
                }
            };
            if file_type.is_symlink() {
                // Listing a symlink name is harmless; payload authorization
                // below still requires either strict symlink-free policy or a
                // validated legacy route. Do not stat/follow its target here.
                files.push(path);
                continue;
            }
            if file_type.is_dir() {
                directories.push(path);
            } else if file_type.is_file() {
                let bytes = match entry.metadata() {
                    Ok(metadata) => metadata.len(),
                    Err(_) => {
                        complete = false;
                        continue;
                    }
                };
                self.scanned_bytes = self.scanned_bytes.saturating_add(bytes as usize);
                if self.scanned_bytes > self.max_scanned_bytes {
                    complete = false;
                    break;
                }
                files.push(path);
            }
        }
        for path in self.overlays.keys() {
            if path
                .parent()
                .is_some_and(|parent| path_equivalent(parent, &directory))
                && !files.iter().any(|existing| path_equivalent(existing, path))
            {
                files.push(path.clone());
            }
        }
        files.sort_by_key(|left| path_key(left));
        directories.sort_by_key(|left| path_key(left));
        Ok(DirectoryListing {
            files,
            directories,
            stamp,
            complete,
        })
    }

    fn overlay_candidates(&self, roots: &[PathBuf], names: &[String]) -> Vec<PathBuf> {
        let mut result = self
            .overlays
            .keys()
            .filter(|path| {
                roots
                    .iter()
                    .any(|root| path_starts_with(path, &canonical_path(root)))
                    && path.file_name().is_some_and(|file_name| {
                        names
                            .iter()
                            .any(|name| file_name.to_string_lossy().eq_ignore_ascii_case(name))
                    })
            })
            .cloned()
            .collect::<Vec<_>>();
        result.sort_by_key(|left| path_key(left));
        result.dedup_by(|left, right| path_equivalent(left, right));
        result
    }

    fn load(
        &mut self,
        request: SourceRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<LoadedSource, SourceStoreError> {
        self.load_inner(request, cancel, true)
    }
}

impl FilesystemSourceStore {
    fn load_inner(
        &mut self,
        request: SourceRequest<'_>,
        cancel: &dyn CancellationToken,
        decode: bool,
    ) -> Result<LoadedSource, SourceStoreError> {
        if cancel.is_cancelled() {
            return Err(SourceStoreError::Cancelled);
        }
        let path = canonical_path(request.path);
        if !path_equivalent(&path, &request.entry.path) {
            return Err(SourceStoreError::Unauthorized {
                path,
                reason: "source and authorization entry identify different paths".to_string(),
            });
        }
        // On a case-insensitive volume any spelling opens the file; report and
        // authorize the one on disk so a file has one identity.
        let path = path_identity::on_disk_spelling(&path).unwrap_or(path);
        let entry = ProjectPathEntry {
            path: path.clone(),
            provenance: request.entry.provenance.clone(),
        };
        if let Some((path, overlay)) = self.overlay_for(&path) {
            let legacy_overlay = matches!(entry.provenance, ProjectPathProvenance::LegacyNative)
                && request.read_policy.allows_legacy_route_entry(&entry);
            if (!legacy_overlay && !request.read_policy.allows_location(&entry))
                || (legacy_overlay && !request.read_policy.allows_legacy_route_entry(&entry))
            {
                return Err(SourceStoreError::Unauthorized {
                    path,
                    reason: "source is outside the requester-scoped read policy".to_string(),
                });
            }
            if overlay.bytes.len() > request.max_bytes {
                return Err(SourceStoreError::TooLarge {
                    path,
                    maximum: request.max_bytes,
                });
            }
            return Ok(LoadedSource {
                id: source_id_for_path(&path),
                path,
                bytes: overlay.bytes.clone(),
                decoded_text: None,
                compile_time_backing: None,
                revision: SourceRevision::Overlay {
                    version: overlay.version,
                    content_hash: content_hash_bytes(&overlay.bytes),
                },
            });
        }

        let legacy_payload = matches!(entry.provenance, ProjectPathProvenance::LegacyNative)
            && request.legacy_route.is_some()
            && request.read_policy.allows_legacy_route_entry(&entry);
        if !legacy_payload && !request.read_policy.allows_location(&entry) {
            return Err(SourceStoreError::Unauthorized {
                path,
                reason: "source is outside the requester-scoped read policy".to_string(),
            });
        }
        let link_metadata = fs::symlink_metadata(&path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                SourceStoreError::NotFound { path: path.clone() }
            } else {
                SourceStoreError::Io {
                    path: path.clone(),
                    message: error.to_string(),
                }
            }
        })?;
        if !legacy_payload && (link_metadata.file_type().is_symlink() || !link_metadata.is_file()) {
            return Err(SourceStoreError::NotRegularFile { path });
        }
        if legacy_payload && !request.read_policy.allows_legacy_payload_entry(&entry) {
            return Err(SourceStoreError::Unauthorized {
                path,
                reason: "legacy payload route was not authorized".to_string(),
            });
        }
        if !legacy_payload && !request.read_policy.allows_entry(&entry) {
            return Err(SourceStoreError::Unauthorized {
                path,
                reason: "payload is not authorized".to_string(),
            });
        }
        let bytes = if legacy_payload {
            request
                .read_policy
                .read_legacy_payload_bytes(&entry, request.max_bytes as u64)
        } else {
            request
                .read_policy
                .read_payload_bytes(&entry, request.max_bytes as u64)
        }
        .map_err(|error| map_payload_error(&path, request.max_bytes, error))?;
        if cancel.is_cancelled() {
            return Err(SourceStoreError::Cancelled);
        }
        if bytes.len() > request.max_bytes {
            return Err(SourceStoreError::TooLarge {
                path,
                maximum: request.max_bytes,
            });
        }
        let stamp = path_stamp_result(&path)
            .map_err(|error| SourceStoreError::Io {
                path: path.clone(),
                message: error.to_string(),
            })?
            .ok_or_else(|| SourceStoreError::NotFound { path: path.clone() })?;
        let content_hash = content_hash_bytes(&bytes);
        let decoded_text =
            decode.then(|| Arc::<str>::from(crate::text::decode_bytes(&bytes).into_owned()));
        Ok(LoadedSource {
            id: source_id_for_path(&path),
            path,
            bytes: Arc::<[u8]>::from(bytes),
            decoded_text,
            compile_time_backing: None,
            revision: SourceRevision::Disk {
                stamp,
                content_hash,
                read_policy: request.read_policy.clone(),
                path_entry: entry,
            },
        })
    }
}

fn map_payload_error(path: &Path, maximum: usize, error: String) -> SourceStoreError {
    let lower = error.to_ascii_lowercase();
    if lower.contains("not authorized") {
        SourceStoreError::Unauthorized {
            path: path.to_path_buf(),
            reason: error,
        }
    } else if lower.contains("exceed") || lower.contains("safety limit") {
        SourceStoreError::TooLarge {
            path: path.to_path_buf(),
            maximum,
        }
    } else if lower.contains("regular file") {
        SourceStoreError::NotRegularFile {
            path: path.to_path_buf(),
        }
    } else {
        SourceStoreError::Io {
            path: path.to_path_buf(),
            message: error,
        }
    }
}

//! Filesystem and overlay source-store implementation for the shared resolver.

use super::{
    CancellationToken, DirectoryListing, DirectoryRequest, LoadedSource, ProjectPathEntry,
    ProjectPathProvenance, ResolverLimits, SourceRequest, SourceRevision, SourceStore,
    SourceStoreError, canonical_path, content_hash_bytes, path_equivalent, path_key,
    path_starts_with, source_id_for_path,
};
use pascal_project::path_stamp_result;
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
        self.overlays
            .get(&path)
            .cloned()
            .map(|overlay| (path, overlay))
    }
}

impl SourceStore for FilesystemSourceStore {
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
            if path.parent().is_some_and(|parent| parent == directory)
                && !files
                    .iter()
                    .any(|existing| canonical_path(existing) == *path)
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
        let decoded_text = crate::text::decode_bytes(&bytes).into_owned();
        Ok(LoadedSource {
            id: source_id_for_path(&path),
            path,
            bytes: Arc::<[u8]>::from(bytes),
            decoded_text: Some(Arc::<str>::from(decoded_text)),
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

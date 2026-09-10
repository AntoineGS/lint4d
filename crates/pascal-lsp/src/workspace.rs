//! Workspace state, bounded source discovery, overlays, diagnostics, and formatting.

use crate::project::{ProjectContext, ProjectOptions};
use crate::{NavigationIndex, NavigationTarget, text};
use fmt4d::FmtConfig;
use globset::{Glob, GlobSet, GlobSetBuilder};
use lsp_types::{
    Diagnostic as LspDiagnostic, DiagnosticSeverity, Location, NumberOrString, Position, Range,
    TextEdit, Url,
};
use pascal_core::{FileInfo, Severity, decode_bytes, parser};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use walkdir::WalkDir;

/// Maximum syntax-tree depth used before invoking the recursive lint/format pipelines.
pub const MAX_TREE_DEPTH: usize = 256;
const DIAGNOSTIC_DEBOUNCE: Duration = Duration::from_millis(250);
const DEFAULT_MAX_FILES: usize = 10_000;
const DEFAULT_MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_MAX_TOTAL_BYTES: usize = 256 * 1024 * 1024;
const MAX_DEPENDENCY_WORK: usize = 256;
const MAX_DIRECTORY_CATALOGUES: usize = 1024;
const MAX_FILENAME_CATALOGUE_ENTRIES: usize = 10_000;
const MAX_FILENAME_CATALOGUES: usize = 256;
const MAX_WORKSPACE_WARNINGS: usize = 256;
const MAX_DELETED_OVERRIDES: usize = 256;

#[derive(Debug, Clone, Copy)]
pub struct ResourceLimits {
    pub max_files: usize,
    pub max_file_bytes: usize,
    pub max_total_bytes: usize,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_files: DEFAULT_MAX_FILES,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct WorkspaceOptions {
    pub source_paths: Vec<String>,
    pub exclude: Vec<String>,
    pub project_file: Option<PathBuf>,
    pub build_config: Option<String>,
    pub platform: Option<String>,
    pub limits: ResourceLimits,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RawWorkspaceOptions {
    #[serde(default)]
    source_paths: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    project_file: Option<PathBuf>,
    build_config: Option<String>,
    platform: Option<String>,
    max_files: Option<usize>,
    max_file_bytes: Option<usize>,
    max_total_bytes: Option<usize>,
}

impl WorkspaceOptions {
    pub fn parse(value: Option<&serde_json::Value>) -> Result<Self, String> {
        let Some(value) = value.filter(|value| !value.is_null()) else {
            return Ok(Self::default());
        };
        let direct: RawWorkspaceOptions = serde_json::from_value(value.clone())
            .map_err(|error| format!("invalid initializationOptions: {error}"))?;
        let nested = value
            .get("pascalLsp")
            .or_else(|| value.get("pascal-lsp"))
            .filter(|nested| nested.is_object())
            .map(|nested| {
                serde_json::from_value::<RawWorkspaceOptions>(nested.clone())
                    .map_err(|error| format!("invalid initializationOptions.pascalLsp: {error}"))
            })
            .transpose()?;

        let raw = nested.unwrap_or(direct);
        let mut limits = ResourceLimits::default();
        if let Some(value) = raw.max_files {
            limits.max_files = bounded_limit(value, 1, DEFAULT_MAX_FILES, "maxFiles")?;
        }
        if let Some(value) = raw.max_file_bytes {
            limits.max_file_bytes =
                bounded_limit(value, 1, DEFAULT_MAX_FILE_BYTES, "maxFileBytes")?;
        }
        if let Some(value) = raw.max_total_bytes {
            limits.max_total_bytes =
                bounded_limit(value, 1, DEFAULT_MAX_TOTAL_BYTES, "maxTotalBytes")?;
        }
        Ok(Self {
            source_paths: raw.source_paths,
            exclude: raw.exclude,
            project_file: raw.project_file,
            build_config: raw.build_config,
            platform: raw.platform,
            limits,
        })
    }
}

fn bounded_limit(
    value: usize,
    minimum: usize,
    maximum: usize,
    name: &str,
) -> Result<usize, String> {
    if value < minimum {
        return Err(format!(
            "initializationOptions.{name} must be at least {minimum}"
        ));
    }
    if value > maximum {
        eprintln!(
            "pascal-lsp: warning: initializationOptions.{name}={value} exceeds the safe limit {maximum}; using {maximum}"
        );
        return Ok(maximum);
    }
    Ok(value)
}

#[derive(Debug, Clone, Copy)]
pub enum FileChange {
    Created,
    Changed,
    Deleted,
}

#[derive(Debug)]
struct OpenDocument {
    text: Option<String>,
    version: i32,
    rejection: Option<String>,
}

struct DiskSource {
    text: String,
    bytes: usize,
    stamp: DiskStamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiskStamp {
    bytes: u64,
    modified: Option<SystemTime>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PathStamp {
    bytes: u64,
    modified: Option<SystemTime>,
    is_dir: bool,
}

#[derive(Debug, Clone, Default)]
struct DirectoryCatalogue {
    stamp: Option<PathStamp>,
    entries: Vec<PathBuf>,
    last_used: u64,
}

#[derive(Debug, Clone, Default)]
struct FilenameCatalogue {
    entries: HashMap<String, Vec<PathBuf>>,
    complete: bool,
    directories: Vec<(PathBuf, Option<PathStamp>)>,
    last_used: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ContextKey {
    project_file: Option<PathBuf>,
    workspace_root: Option<PathBuf>,
    config: Option<String>,
    platform: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct ContextState {
    context: ProjectContext,
    watched_paths: HashMap<PathBuf, Option<PathStamp>>,
}

#[derive(Debug, Clone)]
struct ExcludeMatcher {
    root: PathBuf,
    config_root: PathBuf,
    patterns: Option<GlobSet>,
}

impl ExcludeMatcher {
    fn new(root: &Path, config_root: &Path, patterns: &[String]) -> Self {
        let mut builder = GlobSetBuilder::new();
        let mut valid_pattern_count = 0;
        for pattern in patterns {
            let normalized = pattern.replace('\\', "/");
            match Glob::new(&normalized) {
                Ok(glob) => {
                    builder.add(glob);
                    valid_pattern_count += 1;
                }
                Err(error) => eprintln!(
                    "pascal-lsp: warning: ignoring invalid exclude glob {pattern:?}: {error}"
                ),
            }
        }
        let compiled = if valid_pattern_count == 0 {
            None
        } else {
            match builder.build() {
                Ok(set) => Some(set),
                Err(error) => {
                    eprintln!("pascal-lsp: warning: failed to build exclude globs: {error}");
                    None
                }
            }
        };
        Self {
            root: root.to_path_buf(),
            config_root: config_root.to_path_buf(),
            patterns: compiled,
        }
    }

    fn is_excluded(&self, path: &Path, source_root: &Path) -> bool {
        if path
            .strip_prefix(source_root)
            .is_ok_and(|relative| relative.components().any(is_default_excluded_component))
        {
            return true;
        }

        let Some(patterns) = &self.patterns else {
            return false;
        };
        [source_root, self.root.as_path(), self.config_root.as_path()]
            .into_iter()
            .filter_map(|base| path.strip_prefix(base).ok())
            .any(|relative| {
                let mut prefix = PathBuf::new();
                let mut candidates = Vec::new();
                for component in relative.components() {
                    prefix.push(component.as_os_str());
                    candidates.push(path_to_glob(&prefix));
                }
                candidates
                    .into_iter()
                    .any(|candidate| patterns.is_match(candidate))
            })
    }
}

#[derive(Debug, Clone)]
struct WorkspaceRoot {
    path: PathBuf,
    source_roots: Vec<PathBuf>,
    excludes: ExcludeMatcher,
}

impl WorkspaceRoot {
    fn new(path: PathBuf, options: &WorkspaceOptions) -> Self {
        let path = absolute_path(path);
        let (config_root, config_excludes) = match lint4d::config::Config::discover(&path) {
            Ok((config, config_root)) => (absolute_path(config_root), config.exclude),
            Err(error) => {
                eprintln!(
                    "pascal-lsp: warning: could not read .lint4d.toml under {}: {error}",
                    path.display()
                );
                (path.clone(), Vec::new())
            }
        };
        let mut patterns = options.exclude.clone();
        patterns.extend(config_excludes);
        let mut source_roots = vec![path.clone()];
        for source in &options.source_paths {
            let source_path = PathBuf::from(source);
            let source_path = if source_path.is_absolute() {
                source_path
            } else {
                path.join(source_path)
            };
            let source_path = absolute_path(source_path);
            if !source_roots.contains(&source_path) {
                source_roots.push(source_path);
            }
        }
        Self {
            path: path.clone(),
            source_roots,
            excludes: ExcludeMatcher::new(&path, &config_root, &patterns),
        }
    }

    fn accepts(&self, path: &Path) -> bool {
        let path = absolute_path(path.to_path_buf());
        self.source_roots.iter().any(|source_root| {
            (path.as_path() == source_root.as_path() || path.starts_with(source_root))
                && !self.excludes.is_excluded(&path, source_root)
        })
    }
}

#[derive(Default)]
pub struct Workspace {
    options: WorkspaceOptions,
    roots: Vec<WorkspaceRoot>,
    index: NavigationIndex,
    open_documents: HashMap<Url, OpenDocument>,
    indexed_files: HashSet<Url>,
    indexed_sizes: HashMap<Url, usize>,
    indexed_bytes: usize,
    disk_stamps: HashMap<Url, DiskStamp>,
    last_used: HashMap<Url, u64>,
    use_clock: u64,
    open_text_bytes: usize,
    pending_diagnostics: HashMap<Url, Instant>,
    // Bounded event overrides are needed because some clients report a
    // deletion before the filesystem has caught up. They are cleared by a
    // create/change event or as soon as the observed stamp changes.
    deleted_overrides: HashMap<Url, Option<DiskStamp>>,
    file_cap_warning_sent: bool,
    total_cap_warning_sent: bool,
    contexts: HashMap<ContextKey, ContextState>,
    document_contexts: HashMap<Url, ContextKey>,
    open_document_contexts: HashMap<Url, ContextKey>,
    directory_catalogues: HashMap<PathBuf, DirectoryCatalogue>,
    filename_catalogues: HashMap<PathBuf, FilenameCatalogue>,
    warnings: Vec<String>,
}

impl Workspace {
    pub fn new(roots: Vec<PathBuf>, options: WorkspaceOptions) -> Self {
        let roots = roots
            .into_iter()
            .map(|root| WorkspaceRoot::new(root, &options))
            .collect();
        Self {
            options,
            roots,
            ..Self::default()
        }
    }

    /// Kept as a compatibility no-op for callers of the original workspace
    /// API. Source discovery is request driven; initialization must not parse
    /// an entire workspace.
    pub fn scan(&mut self) {}

    /// Kept as a compatibility no-op. Navigation revalidates only its source
    /// and the dependencies it actually traverses.
    pub fn refresh_for_navigation(&mut self) {}

    /// Resolve a navigation request, loading only the requested source and a
    /// bounded dependency closure.
    pub fn navigate(
        &mut self,
        uri: &Url,
        position: Position,
        target: NavigationTarget,
    ) -> Vec<Location> {
        let Some(context_key) = self.context_for_uri(uri) else {
            return Vec::new();
        };
        if !self.ensure_supported_with_context(uri, &context_key) {
            return Vec::new();
        }
        let empty_pins = HashSet::new();
        if !self.load_source(uri, &context_key, &empty_pins) {
            return Vec::new();
        }

        for _attempt in 0..2 {
            let mut pinned = HashSet::new();
            pinned.insert(uri.clone());
            let locations =
                self.resolve_navigation_once(uri, position, target, &context_key, &mut pinned);
            if !self.revalidate_pinned(&pinned, &context_key) {
                return locations;
            }
        }
        self.warn(format!(
            "navigation source changed repeatedly while resolving {}; result is incomplete",
            uri
        ));
        Vec::new()
    }

    pub fn open_document(&mut self, uri: Url, text: String, version: i32) -> Result<(), String> {
        if !self.open_documents.contains_key(&uri) {
            // A disk dependency may have been indexed under another request's
            // project context. Select the editor buffer's own context when it
            // first becomes authoritative instead of inheriting that binding.
            self.document_contexts.remove(&uri);
        }
        let context_key = self
            .context_for_uri(&uri)
            .ok_or_else(|| format!("could not discover project context for {uri}"))?;
        if !self.ensure_supported_with_context(&uri, &context_key) {
            return Err(format!(
                "document is outside configured source paths: {uri}"
            ));
        }
        if let Some(previous) = self.open_documents.get(&uri) {
            if version <= previous.version {
                eprintln!(
                    "pascal-lsp: warning: ignored non-monotonic version {} for {} (current {})",
                    version, uri, previous.version
                );
                return Ok(());
            }
        }
        if let Err(reason) = self.validate_open_text(&uri, &text) {
            self.reject_open_document(uri, version, reason);
            return Ok(());
        }
        let open_bytes_after = self.open_bytes_after(&uri, text.len());
        let mut pinned = HashSet::new();
        pinned.insert(uri.clone());
        if !self.make_room_for(&uri, text.len(), &pinned, open_bytes_after) {
            let retained_files = self.retained_file_count_after_open(&uri);
            let reason = if retained_files > self.options.limits.max_files {
                format!(
                    "retaining the open document would use {retained_files} files; the configured file limit is {}",
                    self.options.limits.max_files
                )
            } else {
                format!(
                    "retaining the open document would exceed the configured total source limit of {} bytes",
                    self.options.limits.max_total_bytes
                )
            };
            self.reject_open_document(uri, version, reason);
            return Ok(());
        }
        self.accept_open_document(uri, text, version, context_key);
        Ok(())
    }

    pub fn change_document(&mut self, uri: Url, text: String, version: i32) -> Result<(), String> {
        let Some(previous) = self.open_documents.get(&uri) else {
            return Err(format!("received didChange for unopened document {uri}"));
        };
        if version <= previous.version {
            eprintln!(
                "pascal-lsp: warning: ignored non-monotonic version {} for {} (current {})",
                version, uri, previous.version
            );
            return Ok(());
        }
        if let Err(reason) = self.validate_open_text(&uri, &text) {
            self.reject_open_document(uri, version, reason);
            return Ok(());
        }
        let context_key = self
            .context_for_uri(&uri)
            .ok_or_else(|| format!("could not discover project context for {uri}"))?;
        let open_bytes_after = self.open_bytes_after(&uri, text.len());
        let mut pinned = HashSet::new();
        pinned.insert(uri.clone());
        if !self.make_room_for(&uri, text.len(), &pinned, open_bytes_after) {
            let retained_files = self.retained_file_count_after_open(&uri);
            let reason = if retained_files > self.options.limits.max_files {
                format!(
                    "retaining the open document would use {retained_files} files; the configured file limit is {}",
                    self.options.limits.max_files
                )
            } else {
                format!(
                    "retaining the open document would exceed the configured total source limit of {} bytes",
                    self.options.limits.max_total_bytes
                )
            };
            self.reject_open_document(uri, version, reason);
            return Ok(());
        }
        self.accept_open_document(uri, text, version, context_key);
        Ok(())
    }

    pub fn save_document(&mut self, uri: &Url, saved_text: Option<String>) -> Result<(), String> {
        if let Some(document) = self.open_documents.get(uri) {
            if let Some(reason) = &document.rejection {
                return Err(format!("document rejected: {reason}"));
            }
            if let Some(text) = saved_text {
                if let Err(reason) = self.validate_open_text(uri, &text) {
                    let version = document.version;
                    self.reject_open_document(uri.clone(), version, reason);
                    return Ok(());
                }
                let version = document.version;
                let context_key = self
                    .context_for_uri(uri)
                    .ok_or_else(|| format!("could not discover project context for {uri}"))?;
                let open_bytes_after = self.open_bytes_after(uri, text.len());
                let mut pinned = HashSet::new();
                pinned.insert(uri.clone());
                if !self.make_room_for(uri, text.len(), &pinned, open_bytes_after) {
                    return Err(format!(
                        "retaining the saved document would exceed the configured source limits for {uri}"
                    ));
                }
                self.accept_open_document(uri.clone(), text, version, context_key);
            } else {
                self.refresh_loaded_disk(uri);
                self.schedule_diagnostics(uri.clone());
            }
        } else {
            // A save notification without an open overlay must never make the
            // optional text payload authoritative over the disk file.
            self.invalidate_metadata_for_uri(uri);
            self.refresh_loaded_disk(uri);
        }
        Ok(())
    }

    pub fn close_document(&mut self, uri: &Url) -> bool {
        let was_open = if let Some(document) = self.open_documents.remove(uri) {
            if let Some(text) = document.text {
                self.open_text_bytes = self.open_text_bytes.saturating_sub(text.len());
            }
            true
        } else {
            false
        };
        self.pending_diagnostics.remove(uri);
        if was_open {
            self.open_document_contexts.remove(uri);
            self.disk_stamps.remove(uri);
            self.refresh_loaded_disk(uri);
        }
        was_open
    }

    fn accept_open_document(
        &mut self,
        uri: Url,
        text: String,
        version: i32,
        context_key: ContextKey,
    ) {
        let text_len = text.len();
        let source_for_index = text.clone();
        if let Some(previous) = self.open_documents.get(&uri) {
            if let Some(previous_text) = &previous.text {
                self.open_text_bytes = self.open_text_bytes.saturating_sub(previous_text.len());
            }
        }
        self.open_text_bytes = self.open_text_bytes.saturating_add(text_len);
        self.open_documents.insert(
            uri.clone(),
            OpenDocument {
                text: Some(text),
                version,
                rejection: None,
            },
        );
        self.disk_stamps.remove(&uri);
        self.open_document_contexts
            .insert(uri.clone(), context_key.clone());
        self.set_document_context(&uri, &context_key);
        self.index_source(&uri, source_for_index, None, &context_key, &HashSet::new());
        self.schedule_diagnostics(uri);
    }

    fn reject_open_document(&mut self, uri: Url, version: i32, reason: String) {
        if let Some(previous) = self.open_documents.get(&uri) {
            if let Some(previous_text) = &previous.text {
                self.open_text_bytes = self.open_text_bytes.saturating_sub(previous_text.len());
            }
        }
        self.remove_indexed(&uri);
        self.open_document_contexts.remove(&uri);
        self.pending_diagnostics.remove(&uri);
        let message = format!("document rejected: {reason}");
        eprintln!("pascal-lsp: warning: {uri}: {message}");
        self.open_documents.insert(
            uri.clone(),
            OpenDocument {
                text: None,
                version,
                rejection: Some(message),
            },
        );
        self.pending_diagnostics.insert(uri, Instant::now());
    }

    fn validate_open_text(&self, uri: &Url, text: &str) -> Result<(), String> {
        if text.len() > self.options.limits.max_file_bytes {
            return Err(self.file_too_large_message(uri, text.len()));
        }

        let previous_open_bytes = self
            .open_documents
            .get(uri)
            .and_then(|document| document.text.as_ref())
            .map_or(0, String::len);
        let open_bytes_after = self
            .open_text_bytes
            .saturating_sub(previous_open_bytes)
            .saturating_add(text.len());
        if open_bytes_after > self.options.limits.max_total_bytes {
            return Err(format!(
                "open document text uses {open_bytes_after} bytes; the configured total source limit is {}",
                self.options.limits.max_total_bytes
            ));
        }
        Ok(())
    }

    fn open_bytes_after(&self, uri: &Url, incoming_len: usize) -> usize {
        let previous = self
            .open_documents
            .get(uri)
            .and_then(|document| document.text.as_ref())
            .map_or(0, String::len);
        self.open_text_bytes
            .saturating_sub(previous)
            .saturating_add(incoming_len)
    }

    fn retained_file_count_after_open(&self, uri: &Url) -> usize {
        let mut count = self.indexed_files.len();
        for (open_uri, document) in &self.open_documents {
            if open_uri != uri && document.text.is_some() && !self.indexed_files.contains(open_uri)
            {
                count = count.saturating_add(1);
            }
        }
        if !self.indexed_files.contains(uri) {
            count = count.saturating_add(1);
        }
        count
    }

    pub fn file_event(&mut self, uri: &Url, change: FileChange) {
        self.invalidate_metadata_for_uri(uri);
        self.invalidate_directory_for_uri(uri);
        match change {
            FileChange::Deleted => self.remember_deleted(uri),
            FileChange::Created | FileChange::Changed => {
                self.deleted_overrides.remove(uri);
            }
        }
        if self.open_documents.contains_key(uri) {
            // The editor buffer remains authoritative until didClose.
            self.schedule_diagnostics(uri.clone());
            return;
        }
        match change {
            FileChange::Deleted => self.remove_indexed(uri),
            FileChange::Created | FileChange::Changed => self.refresh_loaded_disk(uri),
        }
    }

    pub fn update_workspace_folders(
        &mut self,
        added: impl IntoIterator<Item = PathBuf>,
        removed: impl IntoIterator<Item = PathBuf>,
    ) {
        for removed in removed {
            let removed = absolute_path(removed);
            self.roots
                .retain(|root| !paths_equal_ci(&root.path, &removed));
            let to_remove: Vec<Url> = self
                .indexed_files
                .iter()
                .filter(|uri| {
                    uri.to_file_path()
                        .is_ok_and(|path| path_starts_with_ci(&path, &removed))
                })
                .cloned()
                .collect();
            for uri in to_remove {
                self.remove_indexed(&uri);
            }
        }
        for added in added {
            let added = absolute_path(added);
            if !self.roots.iter().any(|root| root.path == added) {
                self.roots.push(WorkspaceRoot::new(added, &self.options));
            }
        }
        self.contexts.clear();
        self.document_contexts.clear();
        self.open_document_contexts.clear();
        self.directory_catalogues.clear();
        self.filename_catalogues.clear();
        self.deleted_overrides.clear();
    }

    pub fn next_diagnostic_timeout(&self) -> Option<Duration> {
        let now = Instant::now();
        self.pending_diagnostics
            .values()
            .map(|deadline| deadline.saturating_duration_since(now))
            .min()
    }

    pub fn take_due_diagnostics(&mut self) -> Vec<(Url, Option<i32>, Vec<LspDiagnostic>)> {
        let now = Instant::now();
        let due: Vec<Url> = self
            .pending_diagnostics
            .iter()
            .filter_map(|(uri, deadline)| (*deadline <= now).then_some(uri.clone()))
            .collect();
        let mut result = Vec::with_capacity(due.len());
        for uri in due {
            self.pending_diagnostics.remove(&uri);
            let Some(document) = self.open_documents.get(&uri) else {
                continue;
            };
            let version = Some(document.version);
            result.push((uri.clone(), version, self.diagnostics_for(&uri)));
        }
        result
    }

    pub fn formatting_edit(&self, uri: &Url) -> Result<Option<TextEdit>, String> {
        let path = uri
            .to_file_path()
            .map(absolute_path)
            .map_err(|_| format!("not a file URI: {uri}"))?;
        self.ensure_supported_document(uri)?;
        let source = if let Some(document) = self.open_documents.get(uri) {
            if let Some(reason) = &document.rejection {
                return Err(format!("document rejected: {reason}"));
            }
            document
                .text
                .clone()
                .expect("accepted open documents retain their text")
        } else {
            read_disk_source(&path, self.options.limits.max_file_bytes)?.text
        };
        if source.len() > self.options.limits.max_file_bytes {
            return Err(self.file_too_large_message(uri, source.len()));
        }
        ensure_safe_tree_depth(&path, source.as_bytes())?;
        let config = FmtConfig::discover(path.parent().unwrap_or_else(|| Path::new(".")))
            .map_err(|error| error.to_string())?;
        let formatted = fmt4d::format_source(
            source.as_bytes(),
            &FileInfo::new(path.clone()),
            &config,
            &HashSet::new(),
        )
        .map_err(|error| error.to_string())?;
        if formatted == source {
            return Ok(None);
        }
        let end = text::offset_to_position(&source, source.len())
            .ok_or_else(|| "could not compute full-document UTF-16 range".to_string())?;
        Ok(Some(TextEdit::new(
            Range::new(Position::new(0, 0), end),
            formatted,
        )))
    }

    fn refresh_loaded_disk(&mut self, uri: &Url) {
        let Some(context_key) = self.document_contexts.get(uri).cloned() else {
            self.remove_indexed(uri);
            return;
        };
        let pins = HashSet::new();
        self.load_source(uri, &context_key, &pins);
    }

    fn resolve_navigation_once(
        &mut self,
        uri: &Url,
        position: Position,
        target: NavigationTarget,
        context_key: &ContextKey,
        pinned: &mut HashSet<Url>,
    ) -> Vec<Location> {
        // Rebuild the request's bindings from the current source graph. This
        // prevents a deleted or changed dependency from surviving in a stale
        // per-document binding map.
        self.index.clear_import_bindings(uri);
        let mut frontier = vec![uri.clone()];
        let mut visited = HashSet::new();
        let mut initialized = HashSet::from([uri.clone()]);
        let mut work = 0;

        loop {
            let locations = self.index.navigate(uri, position, target);
            if !locations.is_empty() {
                return locations;
            }
            if frontier.is_empty() || work >= MAX_DEPENDENCY_WORK {
                break;
            }

            let mut next = Vec::new();
            for current in frontier.drain(..) {
                if !visited.insert(current.clone()) {
                    continue;
                }
                work += 1;
                let dependencies = self.load_imports(&current, context_key, pinned);
                for dependency in dependencies {
                    if initialized.insert(dependency.clone()) {
                        self.index.clear_import_bindings(&dependency);
                    }
                    next.push(dependency);
                }
                if work >= MAX_DEPENDENCY_WORK {
                    break;
                }
            }
            next.retain(|dependency| !visited.contains(dependency));
            next.sort_by(|left, right| left.as_str().cmp(right.as_str()));
            next.dedup();
            frontier = next;
        }

        if work >= MAX_DEPENDENCY_WORK {
            self.warn(format!(
                "dependency work limit ({MAX_DEPENDENCY_WORK}) reached while resolving {uri}; result is incomplete"
            ));
        }

        self.index.navigate(uri, position, target)
    }

    fn load_imports(
        &mut self,
        uri: &Url,
        context_key: &ContextKey,
        pinned: &mut HashSet<Url>,
    ) -> Vec<Url> {
        let imports = self.index.imports(uri);
        let Some(context) = self
            .contexts
            .get(context_key)
            .map(|state| state.context.clone())
        else {
            return Vec::new();
        };
        let mut bindings = HashMap::new();
        let mut dependencies = Vec::new();
        for import in imports {
            let lookup_name = aliased_unit_name(&context, &import.name);
            let Some(dependency) = self.resolve_unit(
                uri,
                &import.name,
                &lookup_name,
                &context,
                context_key,
                pinned,
            ) else {
                self.warn(format!(
                    "could not resolve unit {} imported by {} (searched project context)",
                    import.name, uri
                ));
                continue;
            };
            bindings.insert(import.name, dependency.clone());
            pinned.insert(dependency.clone());
            if !dependencies.contains(&dependency) {
                dependencies.push(dependency);
            }
        }
        self.index.bind_imports(uri, bindings);
        dependencies
    }

    fn load_source(&mut self, uri: &Url, context_key: &ContextKey, pinned: &HashSet<Url>) -> bool {
        let path = match uri.to_file_path() {
            Ok(path) => absolute_path(path),
            Err(()) => {
                self.warn(format!("cannot load non-file navigation URI: {uri}"));
                return false;
            }
        };
        if !is_pascal_path(&path) {
            self.warn(format!(
                "unsupported Pascal dependency path: {}",
                path.display()
            ));
            return false;
        }

        if let Some(document) = self.open_documents.get(uri) {
            let Some(source) = document.text.clone() else {
                return false;
            };
            if self.index.contains(uri) {
                self.touch(uri);
                self.set_document_context(uri, context_key);
                return true;
            }
            return self.index_source(uri, source, None, context_key, pinned);
        }

        let Some(current_stamp) = disk_stamp(&path) else {
            self.remove_indexed(uri);
            return false;
        };
        if self.deletion_blocks_load(uri, &path) {
            self.remove_indexed(uri);
            return false;
        }
        if self.index.contains(uri) && self.disk_stamps.get(uri) == Some(&current_stamp) {
            self.touch(uri);
            self.set_document_context(uri, context_key);
            return true;
        }
        let source = match read_disk_source(&path, self.options.limits.max_file_bytes) {
            Ok(source) => source,
            Err(error) => {
                self.warn(format!("skipping {}: {error}", path.display()));
                self.remove_indexed(uri);
                return false;
            }
        };
        let stamp = source.stamp;
        let indexed = self.index_source(uri, source.text, Some(source.bytes), context_key, pinned);
        if indexed {
            self.disk_stamps.insert(uri.clone(), stamp);
        }
        indexed
    }

    fn index_source(
        &mut self,
        uri: &Url,
        source: String,
        disk_size: Option<usize>,
        context_key: &ContextKey,
        pinned: &HashSet<Url>,
    ) -> bool {
        let size = disk_size.unwrap_or(source.len());
        if !self.make_room_for(uri, size, pinned, 0) {
            if !self.open_documents.contains_key(uri) {
                self.warn(format!(
                    "source cache limit prevented retaining {}; navigation is incomplete",
                    uri
                ));
            }
            return false;
        }
        if let Err(error) = self.index.update(uri.clone(), source) {
            self.remove_indexed(uri);
            self.warn(format!("cannot index {uri}: {error}"));
            return false;
        }

        let old_size = self.indexed_sizes.insert(uri.clone(), size);
        if let Some(old_size) = old_size {
            self.indexed_bytes = self.indexed_bytes.saturating_sub(old_size);
        } else {
            self.indexed_files.insert(uri.clone());
        }
        self.indexed_bytes = self.indexed_bytes.saturating_add(size);
        self.set_document_context(uri, context_key);
        self.index.clear_import_bindings(uri);
        self.touch(uri);
        true
    }

    fn make_room_for(
        &mut self,
        uri: &Url,
        size: usize,
        pinned: &HashSet<Url>,
        additional_bytes: usize,
    ) -> bool {
        loop {
            let old_size = self.indexed_sizes.get(uri).copied().unwrap_or(0);
            let indexed_after = self
                .indexed_bytes
                .saturating_sub(old_size)
                .saturating_add(size);
            let new_file = !self.indexed_files.contains(uri);
            let files_after = self.indexed_files.len() + usize::from(new_file);
            let over_files = files_after > self.options.limits.max_files;
            let over_bytes = indexed_after.saturating_add(additional_bytes)
                > self.options.limits.max_total_bytes;
            if !over_files && !over_bytes {
                return true;
            }

            let Some(victim) = self.oldest_evictable(pinned, uri) else {
                if over_files && !self.file_cap_warning_sent {
                    self.file_cap_warning_sent = true;
                    self.warn(format!(
                        "file limit ({}) reached while resolving {uri}",
                        self.options.limits.max_files
                    ));
                }
                if over_bytes && !self.total_cap_warning_sent {
                    self.total_cap_warning_sent = true;
                    self.warn(format!(
                        "total source limit ({}) reached while resolving {uri}",
                        self.options.limits.max_total_bytes
                    ));
                }
                return false;
            };
            self.remove_indexed(&victim);
        }
    }

    fn oldest_evictable(&self, pinned: &HashSet<Url>, requested: &Url) -> Option<Url> {
        self.indexed_files
            .iter()
            .filter(|uri| {
                *uri != requested
                    && !pinned.contains(*uri)
                    && !self.open_documents.contains_key(*uri)
            })
            .min_by_key(|uri| self.last_used.get(*uri).copied().unwrap_or(0))
            .cloned()
    }

    fn touch(&mut self, uri: &Url) {
        self.use_clock = self.use_clock.saturating_add(1);
        self.last_used.insert(uri.clone(), self.use_clock);
    }

    fn revalidate_pinned(&mut self, pinned: &HashSet<Url>, context_key: &ContextKey) -> bool {
        let mut changed = false;
        let pins = HashSet::new();
        for uri in pinned {
            if self.open_documents.contains_key(uri) {
                continue;
            }
            let Ok(path) = uri.to_file_path() else {
                continue;
            };
            if disk_stamp(&path) != self.disk_stamps.get(uri).cloned() {
                self.load_source(uri, context_key, &pins);
                changed = true;
            }
        }
        changed
    }

    fn context_for_uri(&mut self, uri: &Url) -> Option<ContextKey> {
        let path = absolute_path(uri.to_file_path().ok()?);
        if !is_pascal_path(&path) {
            self.warn(format!(
                "unsupported Pascal document path: {}",
                path.display()
            ));
            return None;
        }

        let mut rediscover_open_context = false;
        if let Some(existing) = self.open_document_contexts.get(uri).cloned() {
            self.extend_context_watch_paths(&existing, &path);
            if self.context_is_fresh(&existing) {
                return Some(existing);
            }
            self.invalidate_context(&existing);
            rediscover_open_context = true;
        }

        if !rediscover_open_context {
            if let Some(existing) = self.document_contexts.get(uri).cloned() {
                self.extend_context_watch_paths(&existing, &path);
                if self.context_is_fresh(&existing) {
                    return Some(existing);
                }
                self.invalidate_context(&existing);
            }
        }

        let roots = self
            .roots
            .iter()
            .map(|root| root.path.clone())
            .collect::<Vec<_>>();
        let project_options = ProjectOptions {
            project_file: self.options.project_file.clone(),
            build_config: self.options.build_config.clone(),
            platform: self.options.platform.clone(),
            source_paths: self.options.source_paths.clone(),
        };
        let context = match ProjectContext::discover(&path, &roots, &project_options) {
            Ok(context) => context,
            Err(error) => {
                self.warn(format!(
                    "could not discover project context for {path:?}: {error}"
                ));
                return None;
            }
        };
        for warning in context.warnings.iter().cloned() {
            self.warn(warning);
        }
        let key = self.context_key_for_path(&path, Some(&context));
        self.install_context(key.clone(), context, &path);
        self.select_document_context(uri, &key);
        Some(key)
    }

    fn context_key_for_path(&self, path: &Path, context: Option<&ProjectContext>) -> ContextKey {
        ContextKey {
            project_file: context.and_then(|context| context.project_file.clone()),
            workspace_root: self.root_for_path(path),
            config: context.and_then(|context| context.config.clone()),
            platform: context.and_then(|context| context.platform.clone()),
        }
    }

    fn root_for_path(&self, path: &Path) -> Option<PathBuf> {
        self.roots
            .iter()
            .filter(|root| path_starts_with_ci(path, &root.path))
            .max_by_key(|root| root.path.components().count())
            .map(|root| root.path.clone())
    }

    fn install_context(&mut self, key: ContextKey, context: ProjectContext, file: &Path) {
        let mut watched_paths = HashMap::new();
        let mut metadata = context.metadata_files.clone();
        if let Some(project_file) = &context.project_file {
            metadata.push(project_file.clone());
        }
        if let Some(main_source) = &context.main_source {
            metadata.push(main_source.clone());
        }
        metadata.extend(self.discovery_directories(file));
        metadata.sort_by(|left, right| left.to_string_lossy().cmp(&right.to_string_lossy()));
        metadata.dedup_by(|left, right| paths_equal_ci(left, right));
        for path in metadata {
            let stamp = path_stamp(&path);
            watched_paths.insert(path, stamp);
        }
        if let Some(state) = self.contexts.get_mut(&key) {
            state.context = context;
            state.watched_paths.extend(watched_paths);
        } else {
            self.contexts.insert(
                key,
                ContextState {
                    context,
                    watched_paths,
                },
            );
        }
    }

    fn extend_context_watch_paths(&mut self, key: &ContextKey, file: &Path) {
        let paths = self.discovery_directories(file);
        if let Some(state) = self.contexts.get_mut(key) {
            for path in paths {
                state
                    .watched_paths
                    .entry(path.clone())
                    .or_insert_with(|| path_stamp(&path));
            }
        }
    }

    fn discovery_directories(&self, file: &Path) -> Vec<PathBuf> {
        let Some(mut directory) = file.parent().map(Path::to_path_buf) else {
            return Vec::new();
        };
        let boundary = self.root_for_path(file);
        let mut result = Vec::new();
        loop {
            result.push(directory.clone());
            if boundary
                .as_ref()
                .is_some_and(|root| paths_equal_ci(root, &directory))
            {
                break;
            }
            let Some(parent) = directory.parent() else {
                break;
            };
            if parent == directory {
                break;
            }
            directory = parent.to_path_buf();
        }
        result
    }

    fn context_is_fresh(&self, key: &ContextKey) -> bool {
        self.contexts.get(key).is_some_and(|state| {
            state
                .watched_paths
                .iter()
                .all(|(path, stamp)| path_stamp(path) == *stamp)
        })
    }

    fn invalidate_context(&mut self, key: &ContextKey) {
        self.contexts.remove(key);
        let mut affected: HashSet<Url> = self
            .document_contexts
            .iter()
            .filter_map(|(uri, document_key)| (document_key == key).then_some(uri.clone()))
            .collect();
        affected.extend(
            self.open_document_contexts
                .iter()
                .filter_map(|(uri, document_key)| (document_key == key).then_some(uri.clone())),
        );
        for uri in affected {
            self.index.clear_import_bindings(&uri);
            self.document_contexts.remove(&uri);
        }
    }

    fn invalidate_metadata_for_uri(&mut self, uri: &Url) {
        let Ok(path) = uri.to_file_path() else {
            return;
        };
        let path = absolute_path(path);
        let affected: Vec<ContextKey> = self
            .contexts
            .iter()
            .filter_map(|(key, state)| {
                state
                    .watched_paths
                    .keys()
                    .any(|watched| paths_equal_ci(watched, &path))
                    .then_some(key.clone())
            })
            .collect();
        for key in affected {
            self.invalidate_context(&key);
        }
    }

    fn invalidate_directory_for_uri(&mut self, uri: &Url) {
        if let Ok(path) = uri.to_file_path() {
            if let Some(parent) = absolute_path(path).parent() {
                self.directory_catalogues.remove(parent);
            }
        }
        // A filename-only catalogue has no source-content dependency and is
        // cheap to rebuild, while clearing it ensures watcher-less creates and
        // renames are visible without a periodic repository scan.
        self.filename_catalogues.clear();
    }

    fn ensure_supported_with_context(&self, uri: &Url, key: &ContextKey) -> bool {
        let Ok(path) = uri.to_file_path() else {
            return false;
        };
        let path = absolute_path(path);
        if !is_pascal_path(&path) {
            return false;
        }
        if self.accepts_path(&path) {
            return true;
        }
        self.contexts.get(key).is_some_and(|state| {
            let context = &state.context;
            context
                .main_source
                .as_ref()
                .is_some_and(|main| paths_equal_ci(main, &path))
                || context
                    .explicit_units
                    .values()
                    .flatten()
                    .any(|candidate| paths_equal_ci(candidate, &path))
                || context
                    .search_paths
                    .iter()
                    .any(|root| path_starts_with_ci(&path, root))
        })
    }

    fn resolve_unit(
        &mut self,
        current_uri: &Url,
        requested_name: &str,
        lookup_name: &str,
        context: &ProjectContext,
        context_key: &ContextKey,
        pinned: &HashSet<Url>,
    ) -> Option<Url> {
        let mut groups: Vec<Vec<PathBuf>> = Vec::new();
        if let Some(explicit) = context.explicit_units.get(lookup_name) {
            groups.push(explicit.clone());
        }

        let current_directory = current_uri
            .to_file_path()
            .ok()
            .and_then(|path| absolute_path(path).parent().map(Path::to_path_buf));
        if let Some(directory) = current_directory {
            groups.extend(self.directory_unit_candidate_groups(
                &directory,
                lookup_name,
                &context.unit_namespaces,
                context_key,
            ));
        }
        for directory in &context.search_paths {
            groups.extend(self.directory_unit_candidate_groups(
                directory,
                lookup_name,
                &context.unit_namespaces,
                context_key,
            ));
        }
        if context.project_file.is_none() {
            groups.push(self.filename_unit_candidates(lookup_name, context, context_key));
        }

        for candidates in groups {
            let mut paths = candidates;
            let mut unique_paths: Vec<PathBuf> = Vec::with_capacity(paths.len());
            for path in paths.drain(..) {
                if !unique_paths
                    .iter()
                    .any(|existing| paths_equal_ci(existing, &path))
                {
                    unique_paths.push(path);
                }
            }
            let mut valid = Vec::new();
            for path in unique_paths {
                let Ok(candidate_uri) = Url::from_file_path(&path) else {
                    continue;
                };
                if candidate_uri == *current_uri {
                    continue;
                }
                if !self.load_source(&candidate_uri, context_key, pinned) {
                    continue;
                }
                let Some(unit_name) = self.index.unit_name(&candidate_uri) else {
                    continue;
                };
                if unit_name_matches(&unit_name, requested_name, lookup_name, context) {
                    valid.push(candidate_uri);
                }
            }
            valid.sort_by(|left, right| left.as_str().cmp(right.as_str()));
            valid.dedup();
            match valid.len() {
                0 => {}
                1 => return valid.pop(),
                _ => {
                    self.warn(format!(
                        "ambiguous unit {requested_name} in the current project context: {}",
                        valid
                            .iter()
                            .map(Url::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                    return None;
                }
            }
        }
        if context.project_file.is_some()
            && !context.explicit_units.is_empty()
            && !context.explicit_units.contains_key(lookup_name)
        {
            self.warn(format!(
                "unsupported unit filename mapping for {requested_name}: no explicit project filename mapping; declaration filename mismatch cannot be inferred safely"
            ));
        }
        None
    }

    fn directory_unit_candidate_groups(
        &mut self,
        directory: &Path,
        unit_name: &str,
        namespaces: &[String],
        context_key: &ContextKey,
    ) -> Vec<Vec<PathBuf>> {
        let directory = absolute_path(directory.to_path_buf());
        let mut groups = unit_filename_candidate_tiers(unit_name, namespaces)
            .into_iter()
            .map(|names| self.filename_candidates_for_names(&directory, &names, context_key))
            .collect::<Vec<_>>();
        let parts: Vec<&str> = unit_name
            .split('.')
            .filter(|part| !part.is_empty())
            .collect();
        if parts.len() > 1 {
            let nested = parts[..parts.len() - 1]
                .iter()
                .fold(directory.clone(), |path, part| path.join(part));
            if let Some(nested) = resolve_case_insensitive_path(&nested) {
                groups.extend(
                    unit_filename_candidate_tiers(parts.last().copied().unwrap_or_default(), &[])
                        .into_iter()
                        .map(|names| {
                            self.filename_candidates_for_names(&nested, &names, context_key)
                        }),
                );
            }
        }
        groups
    }

    fn filename_candidates_for_names(
        &mut self,
        directory: &Path,
        names: &[String],
        context_key: &ContextKey,
    ) -> Vec<PathBuf> {
        let mut entries = self.directory_entries(directory);
        entries.extend(self.open_document_entries(directory, context_key));
        let mut candidates: Vec<PathBuf> = Vec::new();
        for name in names {
            for path in &entries {
                if path
                    .file_name()
                    .is_some_and(|file_name| file_name.to_string_lossy().eq_ignore_ascii_case(name))
                    && !candidates
                        .iter()
                        .any(|existing| paths_equal_ci(existing, path))
                {
                    candidates.push(path.clone());
                }
            }
        }
        candidates
    }

    fn open_document_entries(&self, directory: &Path, context_key: &ContextKey) -> Vec<PathBuf> {
        self.open_documents
            .iter()
            .filter_map(|(uri, document)| {
                document.text.as_ref()?;
                let path = absolute_path(uri.to_file_path().ok()?);
                let parent = path.parent()?;
                (paths_equal_ci(parent, directory)
                    && self.ensure_supported_with_context(uri, context_key))
                .then_some(path)
            })
            .collect()
    }

    fn directory_entries(&mut self, directory: &Path) -> Vec<PathBuf> {
        let directory = absolute_path(directory.to_path_buf());
        let stamp = path_stamp(&directory);
        if self
            .directory_catalogues
            .get(&directory)
            .is_some_and(|catalogue| catalogue.stamp == stamp)
        {
            if let Some(catalogue) = self.directory_catalogues.get_mut(&directory) {
                self.use_clock = self.use_clock.saturating_add(1);
                catalogue.last_used = self.use_clock;
                return catalogue.entries.clone();
            }
        }

        let entries = fs::read_dir(&directory)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let file_type = entry.file_type().ok()?;
                (file_type.is_file() && !file_type.is_symlink()).then_some(entry.path())
            })
            .collect::<Vec<_>>();
        self.use_clock = self.use_clock.saturating_add(1);
        self.directory_catalogues.insert(
            directory.clone(),
            DirectoryCatalogue {
                stamp,
                entries: entries.clone(),
                last_used: self.use_clock,
            },
        );
        self.trim_directory_catalogues();
        entries
    }

    fn trim_directory_catalogues(&mut self) {
        while self.directory_catalogues.len() > MAX_DIRECTORY_CATALOGUES {
            let Some(victim) = self
                .directory_catalogues
                .iter()
                .min_by_key(|(_, catalogue)| catalogue.last_used)
                .map(|(path, _)| path.clone())
            else {
                break;
            };
            self.directory_catalogues.remove(&victim);
        }
    }

    fn filename_unit_candidates(
        &mut self,
        unit_name: &str,
        context: &ProjectContext,
        context_key: &ContextKey,
    ) -> Vec<PathBuf> {
        let names = unit_filename_candidates(unit_name, &context.unit_namespaces)
            .into_iter()
            .map(|name| name.to_ascii_lowercase())
            .collect::<Vec<_>>();
        let roots = if context.search_paths.is_empty() {
            self.roots
                .iter()
                .map(|root| root.path.clone())
                .collect::<Vec<_>>()
        } else {
            context.search_paths.clone()
        };
        let mut result = Vec::new();
        for root in &roots {
            let catalogue = self.filename_catalogue(root);
            if !catalogue.complete {
                self.warn(format!(
                    "projectless filename catalogue reached its bounded entry limit under {}",
                    root.display()
                ));
            }
            for name in &names {
                if let Some(paths) = catalogue.entries.get(name) {
                    result.extend(paths.iter().cloned());
                }
            }
        }
        for (uri, document) in &self.open_documents {
            if document.text.is_none() || !self.ensure_supported_with_context(uri, context_key) {
                continue;
            }
            let Ok(path) = uri.to_file_path() else {
                continue;
            };
            let path = absolute_path(path);
            let Some(file_name) = path.file_name() else {
                continue;
            };
            if names
                .iter()
                .any(|name| file_name.to_string_lossy().eq_ignore_ascii_case(name))
                && roots.iter().any(|root| path_starts_with_ci(&path, root))
            {
                result.push(path);
            }
        }
        result
    }

    fn filename_catalogue(&mut self, root: &Path) -> FilenameCatalogue {
        let root = absolute_path(root.to_path_buf());
        let fresh = self
            .filename_catalogues
            .get(&root)
            .is_some_and(|catalogue| {
                catalogue
                    .directories
                    .iter()
                    .all(|(path, stamp)| path_stamp(path) == *stamp)
            });
        if fresh {
            self.use_clock = self.use_clock.saturating_add(1);
            if let Some(catalogue) = self.filename_catalogues.get_mut(&root) {
                catalogue.last_used = self.use_clock;
                return catalogue.clone();
            }
        }
        let mut entries: HashMap<String, Vec<PathBuf>> = HashMap::new();
        let mut directories = vec![(root.clone(), path_stamp(&root))];
        let mut visited = 0;
        let mut complete = true;
        let excludes = self
            .roots
            .iter()
            .find(|workspace_root| path_starts_with_ci(&root, &workspace_root.path))
            .map(|workspace_root| workspace_root.excludes.clone());
        for entry in WalkDir::new(&root).follow_links(false).into_iter() {
            if visited >= MAX_FILENAME_CATALOGUE_ENTRIES {
                complete = false;
                break;
            }
            let Ok(entry) = entry else {
                continue;
            };
            visited += 1;
            if entry.file_type().is_dir() && directories.len() < MAX_FILENAME_CATALOGUE_ENTRIES {
                directories.push((entry.path().to_path_buf(), path_stamp(entry.path())));
            }
            if entry.file_type().is_symlink()
                || excludes
                    .as_ref()
                    .is_some_and(|matcher| matcher.is_excluded(entry.path(), &root))
            {
                continue;
            }
            if !entry.file_type().is_file() || !extension_is(entry.path(), "pas") {
                continue;
            }
            if let Some(name) = entry.path().file_name() {
                entries
                    .entry(name.to_string_lossy().to_ascii_lowercase())
                    .or_default()
                    .push(entry.path().to_path_buf());
            }
        }
        self.use_clock = self.use_clock.saturating_add(1);
        let catalogue = FilenameCatalogue {
            entries,
            complete,
            directories,
            last_used: self.use_clock,
        };
        self.filename_catalogues.insert(root, catalogue.clone());
        self.trim_filename_catalogues();
        catalogue
    }

    fn trim_filename_catalogues(&mut self) {
        while self.filename_catalogues.len() > MAX_FILENAME_CATALOGUES {
            let Some(victim) = self
                .filename_catalogues
                .iter()
                .min_by_key(|(_, catalogue)| catalogue.last_used)
                .map(|(path, _)| path.clone())
            else {
                break;
            };
            self.filename_catalogues.remove(&victim);
        }
    }

    fn remove_indexed(&mut self, uri: &Url) {
        self.index.remove(uri);
        self.indexed_files.remove(uri);
        self.last_used.remove(uri);
        self.document_contexts.remove(uri);
        self.index.clear_import_bindings(uri);
        self.disk_stamps.remove(uri);
        if let Some(size) = self.indexed_sizes.remove(uri) {
            self.indexed_bytes = self.indexed_bytes.saturating_sub(size);
        }
        self.prune_unused_contexts();
    }

    fn prune_unused_contexts(&mut self) {
        let used = self
            .document_contexts
            .values()
            .cloned()
            .collect::<HashSet<_>>();
        let open_used = self
            .open_document_contexts
            .values()
            .cloned()
            .collect::<HashSet<_>>();
        self.contexts
            .retain(|key, _| used.contains(key) || open_used.contains(key));
    }

    fn set_document_context(&mut self, uri: &Url, context_key: &ContextKey) {
        if self.open_documents.contains_key(uri) {
            if !self.open_document_contexts.contains_key(uri) {
                self.open_document_contexts
                    .insert(uri.clone(), context_key.clone());
            }
            let selected = self
                .open_document_contexts
                .get(uri)
                .cloned()
                .expect("open document context is initialized");
            self.document_contexts.insert(uri.clone(), selected);
        } else {
            self.document_contexts
                .insert(uri.clone(), context_key.clone());
        }
        self.prune_unused_contexts();
    }

    fn select_document_context(&mut self, uri: &Url, context_key: &ContextKey) {
        if self.open_documents.contains_key(uri) {
            self.open_document_contexts
                .insert(uri.clone(), context_key.clone());
        }
        self.document_contexts
            .insert(uri.clone(), context_key.clone());
        self.prune_unused_contexts();
    }

    fn remember_deleted(&mut self, uri: &Url) {
        if self.deleted_overrides.len() >= MAX_DELETED_OVERRIDES
            && !self.deleted_overrides.contains_key(uri)
        {
            if let Some(victim) = self.deleted_overrides.keys().next().cloned() {
                self.deleted_overrides.remove(&victim);
            }
        }
        let stamp = uri
            .to_file_path()
            .ok()
            .map(|path| disk_stamp(&absolute_path(path)))
            .unwrap_or(None);
        self.deleted_overrides.insert(uri.clone(), stamp);
    }

    fn deletion_blocks_load(&mut self, uri: &Url, path: &Path) -> bool {
        let Some(deleted_stamp) = self.deleted_overrides.get(uri).cloned() else {
            return false;
        };
        if disk_stamp(path) == deleted_stamp {
            return true;
        }
        self.deleted_overrides.remove(uri);
        false
    }

    fn ensure_supported_document(&self, uri: &Url) -> Result<(), String> {
        let path = uri
            .to_file_path()
            .map_err(|_| format!("not a file URI: {uri}"))?;
        if !is_pascal_path(&path) {
            return Err(format!(
                "unsupported Pascal file extension: {}",
                path.display()
            ));
        }
        if !self.accepts_path(&path) {
            return Err(format!(
                "document is outside configured source paths: {uri}"
            ));
        }
        Ok(())
    }

    /// Number of parsed documents currently retained. This intentionally does
    /// not include filename-only catalogue entries.
    pub fn parsed_document_count(&self) -> usize {
        self.index.document_count()
    }

    /// Warnings emitted by lazy project/unit resolution so embedders can
    /// surface conservative incomplete-resolution status without pretending
    /// to provide compiler diagnostics.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    fn warn(&mut self, message: String) {
        if self.warnings.iter().any(|warning| warning == &message) {
            return;
        }
        if self.warnings.len() < MAX_WORKSPACE_WARNINGS {
            eprintln!("pascal-lsp: warning: {message}");
            self.warnings.push(message);
        } else if self.warnings.len() == MAX_WORKSPACE_WARNINGS {
            eprintln!("pascal-lsp: warning: workspace warning limit reached");
            self.warnings
                .push("workspace warning limit reached".to_string());
        }
    }

    fn accepts_path(&self, path: &Path) -> bool {
        self.roots.iter().any(|root| root.accepts(path))
    }

    fn file_too_large_message(&self, uri: &Url, size: usize) -> String {
        format!(
            "document {uri} is {size} bytes; the configured per-file limit is {}",
            self.options.limits.max_file_bytes
        )
    }

    fn schedule_diagnostics(&mut self, uri: Url) {
        self.pending_diagnostics
            .insert(uri, Instant::now() + DIAGNOSTIC_DEBOUNCE);
    }

    fn diagnostics_for(&self, uri: &Url) -> Vec<LspDiagnostic> {
        let Some(document) = self.open_documents.get(uri) else {
            return Vec::new();
        };
        if let Some(rejection) = &document.rejection {
            return vec![server_diagnostic(rejection, DiagnosticSeverity::ERROR)];
        }
        let source = document
            .text
            .as_deref()
            .expect("accepted open documents retain their text");
        let lint_source = normalize_line_endings(source);
        let path = match uri.to_file_path() {
            Ok(path) => path,
            Err(()) => {
                return vec![server_diagnostic(
                    "not a file URI",
                    DiagnosticSeverity::ERROR,
                )];
            }
        };
        if let Err(error) = ensure_safe_tree_depth(&path, lint_source.as_bytes()) {
            return vec![server_diagnostic(&error, DiagnosticSeverity::ERROR)];
        }
        let config =
            match lint4d::config::Config::discover(path.parent().unwrap_or_else(|| Path::new(".")))
            {
                Ok((config, _)) => config,
                Err(error) => return vec![server_diagnostic(&error, DiagnosticSeverity::ERROR)],
            };
        let raw = lint4d::engine::run_lint(&FileInfo::new(path), lint_source.as_bytes(), &config);
        let line_index = DiagnosticLineIndex::new(&lint_source);
        raw.into_iter()
            .map(|diagnostic| {
                let range = line_index.range(
                    diagnostic.line,
                    diagnostic.column,
                    diagnostic.end_line,
                    diagnostic.end_column,
                );
                let severity = match diagnostic.severity {
                    Severity::Error => DiagnosticSeverity::ERROR,
                    Severity::Warning => DiagnosticSeverity::WARNING,
                    Severity::Hint => DiagnosticSeverity::HINT,
                };
                LspDiagnostic::new(
                    range,
                    Some(severity),
                    Some(NumberOrString::String(diagnostic.rule_id)),
                    Some("lint4d".to_string()),
                    diagnostic.message,
                    None,
                    None,
                )
            })
            .collect()
    }
}

fn server_diagnostic(message: &str, severity: DiagnosticSeverity) -> LspDiagnostic {
    LspDiagnostic::new(
        Range::new(Position::new(0, 0), Position::new(0, 0)),
        Some(severity),
        Some(NumberOrString::String("pascal-lsp".to_string())),
        Some("pascal-lsp".to_string()),
        message.to_string(),
        None,
        None,
    )
}

struct DiagnosticLineIndex<'a> {
    source: &'a str,
    lines: Vec<(usize, usize)>,
    utf16_prefix: Vec<usize>,
}

impl<'a> DiagnosticLineIndex<'a> {
    fn new(source: &'a str) -> Self {
        let mut lines = Vec::new();
        let mut start = 0;
        for (index, byte) in source.bytes().enumerate() {
            if byte == b'\n' {
                lines.push((start, index));
                start = index + 1;
            }
        }
        lines.push((start, source.len()));
        let mut utf16_prefix = vec![0; source.len() + 1];
        let mut units = 0;
        for (index, character) in source.char_indices() {
            units += character.len_utf16();
            utf16_prefix[index + character.len_utf8()] = units;
        }
        Self {
            source,
            lines,
            utf16_prefix,
        }
    }

    fn range(&self, line: usize, column: usize, end_line: usize, end_column: usize) -> Range {
        let start = self.position(line, column).unwrap_or(Position::new(0, 0));
        let end = self.position(end_line, end_column).unwrap_or(start);
        Range::new(start, end)
    }

    fn position(&self, line: usize, column: usize) -> Option<Position> {
        let line_number = line.checked_sub(1)?;
        let (start, end) = *self.lines.get(line_number)?;
        let mut byte_offset = column.saturating_sub(1).min(end - start);
        while byte_offset > 0 && !self.source.is_char_boundary(start + byte_offset) {
            byte_offset -= 1;
        }
        let character = self.utf16_prefix[start + byte_offset] - self.utf16_prefix[start];
        Some(Position::new(
            u32::try_from(line_number).ok()?,
            u32::try_from(character).ok()?,
        ))
    }
}

fn normalize_line_endings(source: &str) -> String {
    if !source.as_bytes().contains(&b'\r') {
        return source.to_owned();
    }
    let bytes = source.as_bytes();
    let mut normalized = String::with_capacity(source.len());
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\r' {
            index += 1;
            continue;
        }
        normalized.push_str(&source[start..index]);
        normalized.push('\n');
        index += 1;
        if bytes.get(index) == Some(&b'\n') {
            index += 1;
        }
        start = index;
    }
    normalized.push_str(&source[start..]);
    normalized
}

fn ensure_safe_tree_depth(path: &Path, source: &[u8]) -> Result<(), String> {
    let (tree, _) = parser::parse_file(&FileInfo::new(path.to_path_buf()), source)?;
    let mut pending = vec![(tree.root_node(), 0usize)];
    while let Some((node, depth)) = pending.pop() {
        if depth > MAX_TREE_DEPTH {
            return Err(format!(
                "analysis skipped: syntax tree depth exceeds the safe limit of {MAX_TREE_DEPTH}"
            ));
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            pending.push((child, depth.saturating_add(1)));
        }
    }
    Ok(())
}

fn path_to_glob(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn read_disk_source(path: &Path, max_bytes: usize) -> Result<DiskSource, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if metadata.len() > max_bytes as u64 {
        return Err(format!(
            "{} is {} bytes; the configured per-file limit is {max_bytes}",
            path.display(),
            metadata.len()
        ));
    }

    let file =
        File::open(path).map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if bytes.len() > max_bytes {
        return Err(format!(
            "{} is larger than the configured per-file limit {max_bytes}",
            path.display()
        ));
    }
    let text = decode_bytes(&bytes).into_owned();
    if text.len() > max_bytes {
        return Err(format!(
            "decoded contents of {} exceed the configured per-file limit {max_bytes}",
            path.display()
        ));
    }
    Ok(DiskSource {
        text,
        bytes: bytes.len(),
        stamp: DiskStamp {
            bytes: metadata.len(),
            modified: metadata.modified().ok(),
        },
    })
}

fn disk_stamp(path: &Path) -> Option<DiskStamp> {
    let metadata = fs::metadata(path).ok()?;
    metadata.is_file().then_some(DiskStamp {
        bytes: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

fn absolute_path(path: PathBuf) -> PathBuf {
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir | Component::Normal(_) => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn path_stamp(path: &Path) -> Option<PathStamp> {
    let metadata = fs::metadata(path).ok()?;
    Some(PathStamp {
        bytes: metadata.len(),
        modified: metadata.modified().ok(),
        is_dir: metadata.is_dir(),
    })
}

fn paths_equal_ci(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

fn path_starts_with_ci(path: &Path, root: &Path) -> bool {
    let path_components = path.components().collect::<Vec<_>>();
    let root_components = root.components().collect::<Vec<_>>();
    path_components.len() >= root_components.len()
        && path_components
            .iter()
            .zip(root_components.iter())
            .all(|(path, root)| {
                path.as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&root.as_os_str().to_string_lossy())
            })
}

fn resolve_case_insensitive_path(path: &Path) -> Option<PathBuf> {
    let absolute = absolute_path(path.to_path_buf());
    let mut current = PathBuf::from(std::path::MAIN_SEPARATOR.to_string());
    for component in absolute.components() {
        let Component::Normal(component) = component else {
            continue;
        };
        let wanted = component.to_string_lossy();
        let mut matches = fs::read_dir(&current)
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&wanted)
                    .then_some(entry.path())
            })
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return None;
        }
        current = matches.pop().expect("one path match");
    }
    Some(current)
}

fn aliased_unit_name(context: &ProjectContext, name: &str) -> String {
    context
        .unit_aliases
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(name))
        .map_or_else(
            || name.to_ascii_lowercase(),
            |(_, target)| target.to_ascii_lowercase(),
        )
}

fn unit_filename_candidates(unit_name: &str, namespaces: &[String]) -> Vec<String> {
    unit_filename_candidate_tiers(unit_name, namespaces)
        .into_iter()
        .flatten()
        .collect()
}

fn unit_filename_candidate_tiers(unit_name: &str, namespaces: &[String]) -> Vec<Vec<String>> {
    let mut tiers = Vec::new();
    let short_name = unit_name.rsplit('.').next().unwrap_or(unit_name);
    let mut exact = vec![format!("{unit_name}.pas")];
    if !short_name.eq_ignore_ascii_case(unit_name) {
        exact.push(format!("{short_name}.pas"));
    }
    tiers.push(exact);
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

fn unit_name_matches(
    actual: &str,
    requested: &str,
    lookup: &str,
    context: &ProjectContext,
) -> bool {
    if actual.eq_ignore_ascii_case(lookup) || actual.eq_ignore_ascii_case(requested) {
        return true;
    }
    if lookup.contains('.') {
        return false;
    }
    context
        .unit_namespaces
        .iter()
        .any(|namespace| format!("{namespace}.{lookup}").eq_ignore_ascii_case(actual))
}

fn is_default_excluded_component(component: Component<'_>) -> bool {
    matches!(
        component,
        Component::Normal(name)
            if matches!(
                name.to_str(),
                Some(
                    ".git"
                        | ".worktrees"
                        | ".hg"
                        | ".svn"
                        | ".idea"
                        | ".vscode"
                        | "target"
                        | "node_modules"
                        | "dist"
                        | "build"
                        | "coverage"
                )
            )
    )
}

fn is_pascal_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("pas")
                || extension.eq_ignore_ascii_case("dpr")
                || extension.eq_ignore_ascii_case("dpk")
        })
}

fn extension_is(path: &Path, extension: &str) -> bool {
    path.extension()
        .is_some_and(|value| value.to_string_lossy().eq_ignore_ascii_case(extension))
}

#[cfg(test)]
mod tests {
    use super::{DiagnosticLineIndex, normalize_line_endings};

    #[test]
    fn diagnostic_line_index_reuses_utf16_prefixes_for_unicode_and_bare_cr() {
        let source = normalize_line_endings("😀abc\rdef\r\nghi");
        let index = DiagnosticLineIndex::new(&source);

        assert_eq!(source, "😀abc\ndef\nghi");
        assert_eq!(index.position(1, 5).expect("after emoji").character, 2);
        assert_eq!(index.position(2, 4).expect("second line end").character, 3);
        assert_eq!(index.position(3, 4).expect("third line end").character, 3);
        assert_eq!(index.utf16_prefix.len(), source.len() + 1);
    }

    #[test]
    fn diagnostic_line_index_builds_a_single_long_line_prefix_table() {
        let source = "x".repeat(46 * 1024);
        let index = DiagnosticLineIndex::new(&source);

        assert_eq!(index.lines.len(), 1);
        assert_eq!(index.utf16_prefix.len(), source.len() + 1);
        assert_eq!(
            index
                .position(1, source.len() + 1)
                .expect("long line end")
                .character as usize,
            source.len()
        );
    }
}

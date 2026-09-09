//! Workspace state, bounded source discovery, overlays, diagnostics, and formatting.

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
const DISK_REVALIDATION_BATCH: usize = 128;
const NEW_FILE_RESCAN_INTERVAL: Duration = Duration::from_millis(250);
const NEW_FILE_RESCAN_BATCH: usize = 512;

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
    pub limits: ResourceLimits,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RawWorkspaceOptions {
    #[serde(default)]
    source_paths: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
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

#[derive(Debug)]
struct ScanCursor {
    root_index: usize,
    source_root: PathBuf,
    walker: walkdir::IntoIter,
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
    indexed_order: Vec<Url>,
    indexed_sizes: HashMap<Url, usize>,
    indexed_bytes: usize,
    disk_stamps: HashMap<Url, DiskStamp>,
    open_text_bytes: usize,
    pending_diagnostics: HashMap<Url, Instant>,
    last_rescan: Option<Instant>,
    revalidation_cursor: usize,
    rescan_root_cursor: usize,
    rescan_cursor: Option<ScanCursor>,
    file_cap_warning_sent: bool,
    total_cap_warning_sent: bool,
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

    /// Scan disk after the initialize response has been sent.
    pub fn scan(&mut self) {
        let source_roots: Vec<(usize, PathBuf)> = self
            .roots
            .iter()
            .enumerate()
            .flat_map(|(root_index, root)| {
                root.source_roots
                    .iter()
                    .cloned()
                    .map(move |path| (root_index, path))
            })
            .collect();
        for (root_index, source_root) in source_roots {
            if self.scan_source_root(root_index, source_root) {
                break;
            }
        }
        eprintln!(
            "pascal-lsp: indexed {} Pascal file(s), {} bytes",
            self.indexed_files.len(),
            self.indexed_bytes
        );
        self.last_rescan = Some(Instant::now());
        self.rescan_root_cursor = 0;
        self.rescan_cursor = None;
    }

    /// Revalidate a bounded batch of known disk files and periodically scan for
    /// newly created files. Open overlays remain authoritative throughout.
    pub fn refresh_for_navigation(&mut self) {
        self.revalidate_disk_batch();
        let should_rescan = match self.last_rescan {
            Some(last_rescan) => last_rescan.elapsed() >= NEW_FILE_RESCAN_INTERVAL,
            None => true,
        };
        if should_rescan {
            self.rescan_new_files();
            self.last_rescan = Some(Instant::now());
        }
    }

    /// Resolve a navigation request against the current disk/overlay index.
    pub fn navigate(
        &self,
        uri: &Url,
        position: Position,
        target: NavigationTarget,
    ) -> Vec<Location> {
        self.index.navigate(uri, position, target)
    }

    pub fn open_document(&mut self, uri: Url, text: String, version: i32) -> Result<(), String> {
        self.ensure_supported_document(&uri)?;
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
        self.accept_open_document(uri, text, version);
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
        self.accept_open_document(uri, text, version);
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
                self.accept_open_document(uri.clone(), text, version);
            } else {
                self.upsert_source(uri, None);
                self.schedule_diagnostics(uri.clone());
            }
        } else {
            // A save notification without an open overlay must never make the
            // optional text payload authoritative over the disk file.
            self.refresh_disk(uri);
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
            self.refresh_disk(uri);
        }
        was_open
    }

    fn accept_open_document(&mut self, uri: Url, text: String, version: i32) {
        let text_len = text.len();
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
        self.upsert_source(&uri, None);
        self.schedule_diagnostics(uri);
    }

    fn reject_open_document(&mut self, uri: Url, version: i32, reason: String) {
        if let Some(previous) = self.open_documents.get(&uri) {
            if let Some(previous_text) = &previous.text {
                self.open_text_bytes = self.open_text_bytes.saturating_sub(previous_text.len());
            }
        }
        self.remove_indexed(&uri);
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

        let previous_index_bytes = self.indexed_sizes.get(uri).copied().unwrap_or(0);
        let indexed_bytes_after = self
            .indexed_bytes
            .saturating_sub(previous_index_bytes)
            .saturating_add(text.len());
        if indexed_bytes_after.saturating_add(open_bytes_after)
            > self.options.limits.max_total_bytes
        {
            return Err(format!(
                "retaining the open document would use {} bytes; the configured total source limit is {}",
                indexed_bytes_after.saturating_add(open_bytes_after),
                self.options.limits.max_total_bytes
            ));
        }

        let retained_files = self.retained_file_count_after_open(uri);
        if retained_files > self.options.limits.max_files {
            return Err(format!(
                "retaining the open document would use {retained_files} files; the configured file limit is {}",
                self.options.limits.max_files
            ));
        }
        Ok(())
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
        if self.open_documents.contains_key(uri) {
            // The editor buffer remains authoritative until didClose.
            self.schedule_diagnostics(uri.clone());
            return;
        }
        match change {
            FileChange::Deleted => self.remove_indexed(uri),
            FileChange::Created | FileChange::Changed => self.refresh_disk(uri),
        }
    }

    pub fn update_workspace_folders(
        &mut self,
        added: impl IntoIterator<Item = PathBuf>,
        removed: impl IntoIterator<Item = PathBuf>,
    ) {
        for removed in removed {
            let removed = absolute_path(removed);
            self.roots.retain(|root| root.path != removed);
            let to_remove: Vec<Url> = self
                .indexed_files
                .iter()
                .filter(|uri| {
                    uri.to_file_path()
                        .is_ok_and(|path| path.starts_with(&removed))
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
        self.scan();
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

    fn scan_source_root(&mut self, root_index: usize, source_root: PathBuf) -> bool {
        let Some(root) = self.roots.get(root_index).cloned() else {
            return false;
        };
        if root.excludes.is_excluded(&source_root, &source_root) {
            return false;
        }
        let mut walker = WalkDir::new(&source_root).follow_links(false).into_iter();
        while let Some(entry) = walker.next() {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    eprintln!("pascal-lsp: warning: source scan error: {error}");
                    continue;
                }
            };
            if entry.file_type().is_symlink()
                || root.excludes.is_excluded(entry.path(), &source_root)
            {
                if entry.file_type().is_dir() {
                    walker.skip_current_dir();
                }
                continue;
            }
            if self.index_disk_entry(&entry) {
                return true;
            }
        }
        false
    }

    fn index_disk_entry(&mut self, entry: &walkdir::DirEntry) -> bool {
        if !entry.file_type().is_file() || !is_pascal_path(entry.path()) {
            return false;
        }
        let uri = match Url::from_file_path(entry.path()) {
            Ok(uri) => uri,
            Err(()) => {
                eprintln!(
                    "pascal-lsp: warning: cannot make file URI for {}",
                    entry.path().display()
                );
                return false;
            }
        };
        if self.indexed_files.contains(&uri) || self.open_documents.contains_key(&uri) {
            return false;
        }
        if self.indexed_files.len() >= self.options.limits.max_files {
            if !self.file_cap_warning_sent {
                self.file_cap_warning_sent = true;
                eprintln!(
                    "pascal-lsp: warning: file limit ({}) reached; remaining files were not indexed",
                    self.options.limits.max_files
                );
            }
            return true;
        }
        let source = match read_disk_source(entry.path(), self.options.limits.max_file_bytes) {
            Ok(source) => source,
            Err(error) => {
                eprintln!(
                    "pascal-lsp: warning: skipping {}: {error}",
                    entry.path().display()
                );
                return false;
            }
        };
        if self.indexed_bytes.saturating_add(source.bytes) > self.options.limits.max_total_bytes {
            if !self.total_cap_warning_sent {
                self.total_cap_warning_sent = true;
                eprintln!(
                    "pascal-lsp: warning: total source limit ({}) reached; some files were not indexed",
                    self.options.limits.max_total_bytes
                );
            }
            return false;
        }
        let stamp = source.stamp;
        self.index_source(&uri, source.text, Some(source.bytes));
        if self.indexed_files.contains(&uri) {
            self.disk_stamps.insert(uri, stamp);
        }
        false
    }

    fn rescan_new_files(&mut self) {
        let source_roots: Vec<(usize, PathBuf)> = self
            .roots
            .iter()
            .enumerate()
            .flat_map(|(root_index, root)| {
                root.source_roots
                    .iter()
                    .cloned()
                    .map(move |path| (root_index, path))
            })
            .collect();
        if source_roots.is_empty() {
            return;
        }
        let mut cursor = self.rescan_cursor.take();
        let mut processed = 0;
        while processed < NEW_FILE_RESCAN_BATCH {
            if cursor.is_none() {
                let source_index = self.rescan_root_cursor % source_roots.len();
                self.rescan_root_cursor = (source_index + 1) % source_roots.len();
                let (root_index, source_root) = source_roots[source_index].clone();
                cursor = Some(ScanCursor {
                    root_index,
                    source_root: source_root.clone(),
                    walker: WalkDir::new(source_root).follow_links(false).into_iter(),
                });
            }
            let root_index = cursor
                .as_ref()
                .expect("rescan cursor initialized")
                .root_index;
            let Some(root) = self.roots.get(root_index).cloned() else {
                cursor = None;
                processed += 1;
                continue;
            };
            let entry = {
                let scan = cursor.as_mut().expect("rescan cursor initialized");
                scan.walker.next()
            };
            let Some(entry) = entry else {
                cursor = None;
                processed += 1;
                continue;
            };
            processed += 1;
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    eprintln!("pascal-lsp: warning: source rescan error: {error}");
                    continue;
                }
            };
            let source_root = &cursor
                .as_ref()
                .expect("rescan cursor initialized")
                .source_root;
            if entry.file_type().is_symlink()
                || root.excludes.is_excluded(entry.path(), source_root)
            {
                if entry.file_type().is_dir() {
                    cursor
                        .as_mut()
                        .expect("rescan cursor initialized")
                        .walker
                        .skip_current_dir();
                }
                continue;
            }
            if self.index_disk_entry(&entry) {
                break;
            }
        }
        self.rescan_cursor = cursor;
    }

    fn revalidate_disk_batch(&mut self) {
        if self.indexed_order.is_empty() {
            return;
        }
        let order_len = self.indexed_order.len();
        let start = self.revalidation_cursor % order_len;
        let count = order_len.min(DISK_REVALIDATION_BATCH);
        let batch: Vec<Url> = (0..count)
            .map(|offset| self.indexed_order[(start + offset) % order_len].clone())
            .collect();
        for uri in batch {
            let Some(path) = uri.to_file_path().ok() else {
                self.remove_indexed(&uri);
                continue;
            };
            if self.open_documents.contains_key(&uri) {
                if let Some(stamp) = disk_stamp(&path) {
                    self.disk_stamps.insert(uri, stamp);
                }
                continue;
            }
            let current = disk_stamp(&path);
            if self.disk_stamps.get(&uri) != current.as_ref() {
                self.refresh_disk(&uri);
            }
        }
        if self.indexed_order.is_empty() {
            self.revalidation_cursor = 0;
        } else {
            self.revalidation_cursor = (start + count) % self.indexed_order.len();
        }
    }

    fn refresh_disk(&mut self, uri: &Url) {
        let Ok(path) = uri.to_file_path() else {
            self.remove_indexed(uri);
            return;
        };
        let path = absolute_path(path);
        if !self.accepts_path(&path) || !is_pascal_path(&path) {
            self.remove_indexed(uri);
            return;
        }
        let source = match read_disk_source(&path, self.options.limits.max_file_bytes) {
            Ok(source) => source,
            Err(error) => {
                eprintln!("pascal-lsp: warning: skipping {}: {error}", path.display());
                self.remove_indexed(uri);
                return;
            }
        };
        let old_size = self.indexed_sizes.get(uri).copied().unwrap_or(0);
        if self
            .indexed_bytes
            .saturating_sub(old_size)
            .saturating_add(source.bytes)
            > self.options.limits.max_total_bytes
        {
            if !self.total_cap_warning_sent {
                self.total_cap_warning_sent = true;
                eprintln!(
                    "pascal-lsp: warning: total source limit ({}) prevents refreshing {}",
                    self.options.limits.max_total_bytes,
                    path.display()
                );
            }
            self.remove_indexed(uri);
            return;
        }
        let stamp = source.stamp;
        self.index_source(uri, source.text, Some(source.bytes));
        if self.indexed_files.contains(uri) {
            self.disk_stamps.insert(uri.clone(), stamp);
        }
    }

    fn index_source(&mut self, uri: &Url, source: String, disk_size: Option<usize>) {
        let size = disk_size.unwrap_or(source.len());
        let old_size = self.indexed_sizes.get(uri).copied().unwrap_or(0);
        let is_new = !self.indexed_files.contains(uri);
        if is_new && self.indexed_files.len() >= self.options.limits.max_files {
            if !self.file_cap_warning_sent {
                self.file_cap_warning_sent = true;
                eprintln!(
                    "pascal-lsp: warning: file limit ({}) reached; {uri} was not indexed",
                    self.options.limits.max_files
                );
            }
            return;
        }
        if self
            .indexed_bytes
            .saturating_sub(old_size)
            .saturating_add(size)
            > self.options.limits.max_total_bytes
        {
            if !self.total_cap_warning_sent {
                self.total_cap_warning_sent = true;
                eprintln!(
                    "pascal-lsp: warning: total source limit ({}) prevents indexing {uri}",
                    self.options.limits.max_total_bytes
                );
            }
            if !is_new {
                self.remove_indexed(uri);
            }
            return;
        }
        if let Err(error) = self.index.update(uri.clone(), source) {
            self.remove_indexed(uri);
            eprintln!("pascal-lsp: warning: cannot index {uri}: {error}");
            return;
        }
        if let Some(old_size) = self.indexed_sizes.insert(uri.clone(), size) {
            self.indexed_bytes = self.indexed_bytes.saturating_sub(old_size);
        } else {
            self.indexed_files.insert(uri.clone());
            self.indexed_order.push(uri.clone());
        }
        self.indexed_bytes = self.indexed_bytes.saturating_add(size);
    }

    fn upsert_source(&mut self, uri: &Url, disk_size: Option<usize>) {
        let Some(document) = self.open_documents.get(uri) else {
            return;
        };
        let Some(text) = &document.text else {
            return;
        };
        self.index_source(uri, text.clone(), disk_size);
    }

    fn remove_indexed(&mut self, uri: &Url) {
        self.index.remove(uri);
        self.indexed_files.remove(uri);
        if let Some(index) = self.indexed_order.iter().position(|indexed| indexed == uri) {
            self.indexed_order.swap_remove(index);
            if self.revalidation_cursor > index {
                self.revalidation_cursor -= 1;
            }
        }
        self.disk_stamps.remove(uri);
        if let Some(size) = self.indexed_sizes.remove(uri) {
            self.indexed_bytes = self.indexed_bytes.saturating_sub(size);
        }
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

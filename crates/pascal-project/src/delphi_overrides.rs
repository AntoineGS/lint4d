use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Display;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const LOCAL_CONFIG_NAME: &str = ".delphi-tools.local.toml";
pub const MAX_CONFIG_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PathMapping {
    pub from: String,
    pub to: PathBuf,
    pub config_file: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct EffectiveOverrides {
    pub properties: BTreeMap<String, String>,
    pub property_origins: BTreeMap<String, PathBuf>,
    pub path_mappings: Vec<PathMapping>,
}

impl EffectiveOverrides {
    pub fn visit_recovery_payload(
        &self,
        visit: &mut dyn FnMut(usize) -> Result<(), String>,
    ) -> Result<(), String> {
        for (name, value) in &self.properties {
            visit(name.len())?;
            visit(value.len())?;
        }
        for (name, path) in &self.property_origins {
            visit(name.len())?;
            visit(path.as_os_str().len())?;
        }
        for mapping in &self.path_mappings {
            visit(mapping.from.len())?;
            visit(mapping.to.as_os_str().len())?;
            visit(mapping.config_file.as_os_str().len())?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPath {
    pub path: PathBuf,
    pub mapping: Option<PathMapping>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverrideLayer {
    pub(crate) properties: BTreeMap<String, String>,
    pub(crate) path_mappings: Vec<PathMapping>,
    pub(crate) config_file: PathBuf,
}

pub(crate) type CapturedLayer =
    Result<Option<crate::installation_config::ConfigurationLayer>, String>;

#[derive(Debug, Clone)]
pub struct OverrideSession {
    pub(crate) user_config_file: Option<PathBuf>,
    captured: Arc<Mutex<BTreeMap<PathBuf, CapturedLayer>>>,
    captured_error_stamps:
        Arc<Mutex<BTreeMap<PathBuf, Option<crate::installation_config::ConfigurationSourceStamp>>>>,
    dirty: Arc<Mutex<HashSet<PathBuf>>>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawOverrideFile {
    #[serde(default)]
    properties: BTreeMap<String, String>,
    #[serde(default)]
    path_mappings: Vec<RawPathMapping>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawPathMapping {
    pub(crate) from: String,
    pub(crate) to: String,
}

impl OverrideLayer {
    pub fn parse(text: &str, config_file: &Path) -> Result<Self, String> {
        let raw: RawOverrideFile = toml::from_str(text)
            .map_err(|error| config_error(config_file, format_args!("failed to parse: {error}")))?;

        Self::from_parts(raw.properties, raw.path_mappings, config_file)
    }

    pub(crate) fn from_parts(
        raw_properties: BTreeMap<String, String>,
        raw_mappings: Vec<RawPathMapping>,
        config_file: &Path,
    ) -> Result<Self, String> {
        let mut properties = BTreeMap::new();
        for (name, value) in raw_properties {
            let canonical_name = name.to_ascii_lowercase();
            if !is_valid_property_name(&name) {
                return Err(config_error(
                    config_file,
                    format_args!("invalid property name `{name}`"),
                ));
            }
            if matches!(
                canonical_name.as_str(),
                "thisfiledirectory" | "msbuildthisfiledirectory"
            ) {
                return Err(config_error(
                    config_file,
                    format_args!("property `{name}` is reserved"),
                ));
            }
            if value.contains("$(") {
                return Err(config_error(
                    config_file,
                    format_args!("property `{name}` may not contain `$(`"),
                ));
            }
            if properties.insert(canonical_name, value).is_some() {
                return Err(config_error(
                    config_file,
                    format_args!("duplicate property name `{name}`"),
                ));
            }
        }

        let mut path_mappings = Vec::with_capacity(raw_mappings.len());
        let mut mapping_prefixes = BTreeSet::new();
        for mapping in raw_mappings {
            let from = canonical_mapping_prefix(&mapping.from, config_file)?;
            validate_mapping_destination(&mapping.to, config_file)?;
            if !mapping_prefixes.insert(from.clone()) {
                return Err(config_error(
                    config_file,
                    format_args!("duplicate normalized mapping prefix `{from}`"),
                ));
            }
            path_mappings.push(PathMapping {
                from,
                to: mapping.to.into(),
                config_file: config_file.to_path_buf(),
            });
        }

        Ok(Self {
            properties,
            path_mappings,
            config_file: config_file.to_path_buf(),
        })
    }
}

impl EffectiveOverrides {
    pub fn merge(layers: &[OverrideLayer]) -> Self {
        let mut result = Self::default();
        let mut mappings = BTreeMap::new();
        for layer in layers {
            for (name, value) in &layer.properties {
                result.properties.insert(name.clone(), value.clone());
                result
                    .property_origins
                    .insert(name.clone(), layer.config_file.clone());
            }
            for mapping in &layer.path_mappings {
                mappings.insert(mapping.from.clone(), mapping.clone());
            }
        }
        result.path_mappings = mappings.into_values().collect();
        result
    }

    pub fn resolve_path(&self, raw: &str, base: &Path) -> Result<ResolvedPath, String> {
        let input_components = windows_components(raw)?;
        let Some(input_components) = input_components else {
            return Ok(ResolvedPath {
                path: native_path(raw, base),
                mapping: None,
            });
        };

        let selected = self
            .path_mappings
            .iter()
            .filter_map(|mapping| {
                let prefix = canonical_mapping_components(&mapping.from)?;
                if matches_components(&prefix, &input_components) {
                    Some((prefix.len(), mapping))
                } else {
                    None
                }
            })
            .max_by_key(|(length, _)| *length);

        if let Some((prefix_length, mapping)) = selected {
            if input_components
                .iter()
                .skip(prefix_length)
                .any(|component| introduces_native_path_prefix_or_root(component))
            {
                return Err(format!(
                    "mapped Windows path contains a native path prefix or root in its suffix: {raw}"
                ));
            }

            let mut path = mapping.to.clone();
            for component in input_components.iter().skip(prefix_length) {
                path.push(component);
            }
            return Ok(ResolvedPath {
                path,
                mapping: Some(mapping.clone()),
            });
        }

        #[cfg(unix)]
        {
            Err(format!("Windows path is unavailable on Linux: {raw}"))
        }
        #[cfg(windows)]
        {
            Ok(ResolvedPath {
                path: PathBuf::from(raw),
                mapping: None,
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(format!("Windows path has no native mapping: {raw}"))
        }
    }

    pub fn read_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        for mapping in &self.path_mappings {
            if !roots.contains(&mapping.to) {
                roots.push(mapping.to.clone());
            }
        }
        roots
    }
}

pub fn user_config_path(xdg: Option<&Path>, home: Option<&Path>) -> Result<PathBuf, String> {
    let xdg = xdg.filter(|path| !path.as_os_str().is_empty());
    if let Some(xdg) = xdg.filter(|path| path.is_absolute()) {
        return normalize_absolute_lexical(&xdg.join("delphi-tools").join("config.toml"));
    }

    let home = home
        .filter(|path| !path.as_os_str().is_empty())
        .filter(|path| path.is_absolute())
        .ok_or_else(|| {
            "could not resolve user configuration: HOME must be a non-empty absolute path"
                .to_owned()
        })?;
    normalize_absolute_lexical(
        &home
            .join(".config")
            .join("delphi-tools")
            .join("config.toml"),
    )
}

impl OverrideSession {
    pub fn new(user_config_file: Option<PathBuf>) -> Self {
        let user_config_file =
            user_config_file.map(|path| normalize_absolute_lexical(&path).unwrap_or(path));
        let session = Self {
            user_config_file,
            captured: Arc::new(Mutex::new(BTreeMap::new())),
            captured_error_stamps: Arc::new(Mutex::new(BTreeMap::new())),
            dirty: Arc::new(Mutex::new(HashSet::new())),
        };
        if let Some(user_config_file) = session.user_config_file.as_ref() {
            let _ = session.capture_path(user_config_file);
        }
        session
    }

    pub fn capture_workspace(&self, root: &Path) -> Result<(), String> {
        let root = normalize_absolute_lexical(root)?;
        self.capture_path(&root.join(LOCAL_CONFIG_NAME))
    }

    /// Replace one previously captured configuration with a bounded, cancellable
    /// read. File notifications use this explicit refresh path so ordinary
    /// `effective_for` calls retain their session-capture semantics.
    pub fn refresh_path_with_budget(
        &self,
        path: &Path,
        budget: &dyn crate::ProjectWorkBudget,
    ) -> Result<(), String> {
        let path = normalize_absolute_lexical(path)?;
        let (result, source_stamp) = read_override_file_with_stamp_and_budget(&path, Some(budget));
        let mut captured = self.captured.lock().map_err(|_| capture_store_poisoned())?;
        match result {
            Ok(layer) => {
                captured.insert(path.clone(), Ok(layer));
                self.captured_error_stamps
                    .lock()
                    .map_err(|_| capture_store_poisoned())?
                    .remove(&path);
                self.dirty
                    .lock()
                    .map_err(|_| capture_store_poisoned())?
                    .remove(&path);
                Ok(())
            }
            Err(error) if budget.is_transient_error(&error) => {
                // Keep the last proven value but force the next effective
                // lookup to retry instead of treating this interrupted read
                // as a stable configuration error or current capture.
                self.dirty
                    .lock()
                    .map_err(|_| capture_store_poisoned())?
                    .insert(path);
                Err(error)
            }
            Err(error) => {
                captured.insert(path.clone(), Err(error.clone()));
                self.captured_error_stamps
                    .lock()
                    .map_err(|_| capture_store_poisoned())?
                    .insert(path.clone(), source_stamp);
                self.dirty
                    .lock()
                    .map_err(|_| capture_store_poisoned())?
                    .remove(&path);
                Err(error)
            }
        }
    }

    /// Remove a deleted configuration from the captured session without
    /// consulting the filesystem (the old bytes may still exist until unlink).
    pub fn remove_path(&self, path: &Path) -> Result<(), String> {
        let path = normalize_absolute_lexical(path)?;
        let mut captured = self.captured.lock().map_err(|_| capture_store_poisoned())?;
        captured.insert(path.clone(), Ok(None));
        self.captured_error_stamps
            .lock()
            .map_err(|_| capture_store_poisoned())?
            .remove(&path);
        self.dirty
            .lock()
            .map_err(|_| capture_store_poisoned())?
            .remove(&path);
        Ok(())
    }

    /// Force the next lookup of every captured configuration to re-read it
    /// from disk, for callers that lost track of which files changed.
    pub fn mark_all_dirty(&self) -> Result<(), String> {
        let captured = self.captured.lock().map_err(|_| capture_store_poisoned())?;
        self.dirty
            .lock()
            .map_err(|_| capture_store_poisoned())?
            .extend(captured.keys().cloned());
        Ok(())
    }

    /// Check whether any already captured, usable layer still matches a fresh
    /// source-stamp baseline. Uncaptured paths remain eligible for their first
    /// read. Cached parse errors retain their established reporting behavior
    /// only while the error's captured raw-file stamp still matches baseline.
    pub fn captured_sources_match(
        &self,
        expected: &[crate::installation_config::ConfigurationSourceStamp],
    ) -> Result<bool, String> {
        let captured = self.captured.lock().map_err(|_| capture_store_poisoned())?;
        let error_stamps = self
            .captured_error_stamps
            .lock()
            .map_err(|_| capture_store_poisoned())?;
        for stamp in expected {
            let path = normalize_absolute_lexical(&stamp.path)?;
            let Some(layer) = captured.get(&path) else {
                continue;
            };
            match layer {
                Ok(Some(layer)) if layer.source_stamp != *stamp => return Ok(false),
                Ok(None) if stamp.byte_len.is_some() || stamp.content_hash.is_some() => {
                    return Ok(false);
                }
                Err(_) if error_stamps.get(&path) == Some(&Some(stamp.clone())) => {}
                Err(_) => return Ok(false),
                Ok(Some(_)) | Ok(None) => {}
            }
        }
        Ok(true)
    }

    pub fn effective_for(
        &self,
        workspace_root: Option<&Path>,
        project_directory: Option<&Path>,
    ) -> Result<EffectiveOverrides, String> {
        let mut paths = Vec::with_capacity(3);
        if let Some(user_config_file) = self.user_config_file.as_ref() {
            push_unique_path(&mut paths, user_config_file.clone());
        }
        if let Some(workspace_root) = workspace_root {
            let workspace_root = normalize_absolute_lexical(workspace_root)?;
            push_unique_path(&mut paths, workspace_root.join(LOCAL_CONFIG_NAME));
        }
        if let Some(project_directory) = project_directory {
            let project_directory = normalize_absolute_lexical(project_directory)?;
            push_unique_path(&mut paths, project_directory.join(LOCAL_CONFIG_NAME));
        }

        let mut layers = Vec::with_capacity(paths.len());
        for path in paths {
            self.capture_path(&path)?;
            match self.captured_layer(&path)? {
                Ok(Some(layer)) => layers.push(layer.shared),
                Ok(None) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(EffectiveOverrides::merge(&layers))
    }

    pub(crate) fn capture_path(&self, path: &Path) -> Result<(), String> {
        self.capture_path_with_work_budget(path, None)
    }

    pub(crate) fn capture_path_with_work_budget(
        &self,
        path: &Path,
        budget: Option<&dyn crate::ProjectWorkBudget>,
    ) -> Result<(), String> {
        let path = normalize_absolute_lexical(path)?;
        if let Some(budget) = budget {
            budget.check_cancelled()?;
            budget.charge_path_visits(1)?;
        }
        let mut captured = self.captured.lock().map_err(|_| capture_store_poisoned())?;
        let dirty = self
            .dirty
            .lock()
            .map_err(|_| capture_store_poisoned())?
            .contains(&path);
        if !dirty && let Some(result) = captured.get(&path) {
            return match result {
                Ok(_) => Ok(()),
                Err(error) => Err(error.clone()),
            };
        }

        let (result, source_stamp) = read_override_file_with_stamp_and_budget(&path, budget);
        captured.insert(path.clone(), result.clone());
        match &result {
            Err(_) => {
                self.captured_error_stamps
                    .lock()
                    .map_err(|_| capture_store_poisoned())?
                    .insert(path.clone(), source_stamp);
            }
            Ok(_) => {
                self.captured_error_stamps
                    .lock()
                    .map_err(|_| capture_store_poisoned())?
                    .remove(&path);
            }
        }
        self.dirty
            .lock()
            .map_err(|_| capture_store_poisoned())?
            .remove(&path);
        result.map(|_| ())
    }

    pub(crate) fn captured_layer(&self, path: &Path) -> Result<CapturedLayer, String> {
        let captured = self.captured.lock().map_err(|_| capture_store_poisoned())?;
        captured
            .get(path)
            .cloned()
            .ok_or_else(|| format!("configuration path was not captured: {}", path.display()))
    }
}

impl Default for OverrideSession {
    fn default() -> Self {
        Self::new(None)
    }
}

pub(crate) fn read_override_file_with_budget(
    path: &Path,
    budget: Option<&dyn crate::ProjectWorkBudget>,
) -> CapturedLayer {
    read_override_file_with_stamp_and_budget(path, budget).0
}

fn read_override_file_with_stamp_and_budget(
    path: &Path,
    budget: Option<&dyn crate::ProjectWorkBudget>,
) -> (
    CapturedLayer,
    Option<crate::installation_config::ConfigurationSourceStamp>,
) {
    let mut source_stamp = None;
    let result = (|| {
        if let Some(budget) = budget {
            budget.check_cancelled()?;
            budget.charge_path_visits(1)?;
        }
        let Some(metadata) = inspect_candidate(path, budget)? else {
            return Ok(None);
        };
        if metadata.len() > MAX_CONFIG_BYTES as u64 {
            return Err(format!(
                "{} exceeds {MAX_CONFIG_BYTES} bytes",
                path.display()
            ));
        }

        #[cfg(all(test, target_os = "linux"))]
        maybe_substitute_candidate_after_inspection(path);

        let file = open_candidate(path)
            .map_err(|error| format!("could not open {}: {error}", path.display()))?;
        if let Some(budget) = budget {
            budget.check_cancelled()?;
            budget.charge_path_visits(1)?;
        }
        let opened_metadata = file
            .metadata()
            .map_err(|error| format!("could not inspect opened {}: {error}", path.display()))?;
        validate_regular_file(path, &opened_metadata)?;

        let read_limit = metadata.len().min(MAX_CONFIG_BYTES as u64);
        if let Some(budget) = budget {
            let reserve = usize::try_from(read_limit)
                .map_err(|_| "configuration size does not fit the work budget".to_string())?
                .saturating_add(1);
            budget.ensure_file_read_fits(reserve)?;
            budget.check_cancelled()?;
        }
        let mut bytes = Vec::new();
        let mut reader = file.take(read_limit.saturating_add(1));
        let mut chunk = [0_u8; 8 * 1024];
        loop {
            if let Some(budget) = budget {
                budget.check_cancelled()?;
            }
            let read = reader
                .read(&mut chunk)
                .map_err(|error| format!("could not read {}: {error}", path.display()))?;
            if read == 0 {
                break;
            }
            if let Some(budget) = budget {
                budget.charge_file_bytes(read)?;
            }
            bytes.extend_from_slice(&chunk[..read]);
        }
        if bytes.len() as u64 > read_limit {
            return Err(format!(
                "{} grew beyond its {read_limit} byte read limit",
                path.display(),
            ));
        }
        source_stamp = Some(crate::installation_config::ConfigurationSourceStamp {
            path: path.to_path_buf(),
            byte_len: Some(bytes.len() as u64),
            content_hash: Some(crate::content_hash_bytes(&bytes)),
        });
        let text = std::str::from_utf8(&bytes)
            .map_err(|error| format!("invalid UTF-8 in {}: {error}", path.display()))?;
        crate::installation_config::ConfigurationLayer::parse(text, path).map(Some)
    })();
    (result, source_stamp)
}

pub(crate) fn override_source_stamp_with_budget(
    path: &Path,
    budget: Option<&dyn crate::ProjectWorkBudget>,
) -> Result<Option<crate::installation_config::ConfigurationSourceStamp>, String> {
    if let Some(budget) = budget {
        budget.check_cancelled()?;
        budget.charge_path_visits(1)?;
    }
    let Some(metadata) = inspect_candidate(path, budget)? else {
        return Ok(None);
    };
    if metadata.len() > MAX_CONFIG_BYTES as u64 {
        return Err(format!(
            "{} exceeds {MAX_CONFIG_BYTES} bytes",
            path.display()
        ));
    }
    #[cfg(all(test, target_os = "linux"))]
    maybe_substitute_candidate_after_inspection(path);
    let file = open_candidate(path)
        .map_err(|error| format!("could not open {}: {error}", path.display()))?;
    if let Some(budget) = budget {
        budget.check_cancelled()?;
        budget.charge_path_visits(1)?;
    }
    let opened_metadata = file
        .metadata()
        .map_err(|error| format!("could not inspect opened {}: {error}", path.display()))?;
    validate_regular_file(path, &opened_metadata)?;
    let read_limit = metadata.len().min(MAX_CONFIG_BYTES as u64);
    if let Some(budget) = budget {
        let reserve = usize::try_from(read_limit)
            .map_err(|_| "configuration size does not fit the work budget".to_string())?
            .saturating_add(1);
        budget.ensure_file_read_fits(reserve)?;
        budget.check_cancelled()?;
    }
    let mut bytes = Vec::new();
    let mut reader = file.take(read_limit.saturating_add(1));
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        if let Some(budget) = budget {
            budget.check_cancelled()?;
        }
        let read = reader
            .read(&mut chunk)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        if let Some(budget) = budget {
            budget.charge_file_bytes(read)?;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    if bytes.len() as u64 > read_limit {
        return Err(format!(
            "{} grew beyond its {read_limit} byte read limit",
            path.display()
        ));
    }
    Ok(Some(crate::installation_config::ConfigurationSourceStamp {
        path: path.to_path_buf(),
        byte_len: Some(bytes.len() as u64),
        content_hash: Some(crate::content_hash_bytes(&bytes)),
    }))
}

fn inspect_candidate(
    path: &Path,
    budget: Option<&dyn crate::ProjectWorkBudget>,
) -> Result<Option<fs::Metadata>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not inspect {}: {error}", path.display())),
    };
    let metadata = if metadata.file_type().is_symlink() {
        if let Some(budget) = budget {
            budget.check_cancelled()?;
            budget.charge_path_visits(1)?;
        }
        fs::metadata(path)
            .map_err(|error| format!("could not inspect {}: {error}", path.display()))?
    } else {
        metadata
    };
    validate_regular_file(path, &metadata)?;
    Ok(Some(metadata))
}

#[cfg(all(test, target_os = "linux"))]
static FIFO_SUBSTITUTION_HOOK: std::sync::OnceLock<Mutex<Option<PathBuf>>> =
    std::sync::OnceLock::new();

#[cfg(all(test, target_os = "linux"))]
struct FifoSubstitutionHookGuard;

#[cfg(all(test, target_os = "linux"))]
fn install_fifo_substitution_hook(path: &Path) -> FifoSubstitutionHookGuard {
    let mut hook = FIFO_SUBSTITUTION_HOOK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("FIFO substitution hook lock");
    assert!(hook.replace(path.to_path_buf()).is_none());
    FifoSubstitutionHookGuard
}

#[cfg(all(test, target_os = "linux"))]
impl Drop for FifoSubstitutionHookGuard {
    fn drop(&mut self) {
        FIFO_SUBSTITUTION_HOOK
            .get_or_init(|| Mutex::new(None))
            .lock()
            .expect("FIFO substitution hook lock")
            .take();
    }
}

#[cfg(all(test, target_os = "linux"))]
fn maybe_substitute_candidate_after_inspection(path: &Path) {
    let mut hook = FIFO_SUBSTITUTION_HOOK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("FIFO substitution hook lock");
    let should_substitute = hook.as_ref().is_some_and(|expected| expected == path);
    if should_substitute {
        hook.take();
    }
    if !should_substitute {
        return;
    }
    fs::remove_file(path).expect("remove inspected candidate");
    let status = std::process::Command::new("mkfifo")
        .arg(path)
        .status()
        .expect("create substituted FIFO");
    assert!(status.success(), "mkfifo failed for {}", path.display());
}

fn validate_regular_file(path: &Path, metadata: &fs::Metadata) -> Result<(), String> {
    if metadata.is_dir() {
        return Err(format!("{} is a directory", path.display()));
    }
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_candidate(path: &Path) -> io::Result<File> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;

    // Linux's UAPI O_NONBLOCK value prevents a replaced FIFO from blocking
    // between the candidate inspection and the open.
    const O_NONBLOCK: i32 = 0o4000;
    OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open(path)
}

#[cfg(not(target_os = "linux"))]
fn open_candidate(path: &Path) -> io::Result<File> {
    File::open(path)
}

pub(crate) fn normalize_absolute_lexical(path: &Path) -> Result<PathBuf, String> {
    let absolute = std::path::absolute(path).map_err(|error| {
        format!(
            "could not resolve {} as an absolute path: {error}",
            path.display()
        )
    })?;

    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            Component::Normal(component) => normalized.push(component),
        }
    }
    if !normalized.is_absolute() {
        return Err(format!(
            "could not resolve {} as a native absolute path",
            path.display()
        ));
    }
    Ok(normalized)
}

pub(crate) fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

fn capture_store_poisoned() -> String {
    "Delphi override configuration capture store is poisoned".to_owned()
}

fn canonical_mapping_prefix(raw: &str, config_file: &Path) -> Result<String, String> {
    if contains_parent_component(raw) {
        return Err(config_error(
            config_file,
            format_args!("mapping source prefix may not contain `..`: `{raw}`"),
        ));
    }

    let components = windows_components(raw)
        .map_err(|error| config_error(config_file, error))?
        .ok_or_else(|| {
            config_error(
                config_file,
                format_args!(
                    "mapping source prefix must be an absolute Windows drive or UNC path: `{raw}`"
                ),
            )
        })?;
    let canonical = components
        .iter()
        .map(|component| component.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join("/");
    if canonical.is_empty() {
        return Err(config_error(
            config_file,
            format_args!("mapping source prefix may not be empty: `{raw}`"),
        ));
    }
    Ok(canonical)
}

fn validate_mapping_destination(raw: &str, config_file: &Path) -> Result<(), String> {
    if raw.is_empty() {
        return Err(config_error(
            config_file,
            "mapping destination must be an absolute native path",
        ));
    }
    if raw.contains("$(") || raw.contains("${") || raw.starts_with('~') {
        return Err(config_error(
            config_file,
            format_args!("mapping destination may not contain variables or `~`: `{raw}`"),
        ));
    }
    if contains_parent_component(raw) {
        return Err(config_error(
            config_file,
            format_args!("mapping destination may not contain `..`: `{raw}`"),
        ));
    }
    if !Path::new(raw).is_absolute() {
        return Err(config_error(
            config_file,
            format_args!("mapping destination must be an absolute native path: `{raw}`"),
        ));
    }
    Ok(())
}

fn canonical_mapping_components(raw: &str) -> Option<Vec<String>> {
    let bytes = raw.as_bytes();
    if bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Some(vec![raw.to_owned()]);
    }
    windows_components(raw).ok().flatten()
}

fn windows_components(raw: &str) -> Result<Option<Vec<String>>, String> {
    if raw.is_empty() {
        return Ok(None);
    }
    if is_device_path(raw) {
        return Err(format!("device Windows paths are unsupported: {raw}"));
    }

    let bytes = raw.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        if bytes.len() == 2 || !is_separator_byte(bytes[2]) {
            return Err(format!(
                "drive-relative Windows paths are unsupported: {raw}"
            ));
        }

        let mut components = vec![format!("{}:", (bytes[0] as char).to_ascii_lowercase())];
        append_normalized_components(raw[2..].split(is_separator), &mut components, raw)?;
        return Ok(Some(components));
    }

    let leading_separators = bytes
        .iter()
        .take_while(|byte| is_separator_byte(**byte))
        .count();
    if leading_separators >= 2 {
        let remainder = &raw[leading_separators..];
        let mut parts = remainder
            .split(is_separator)
            .filter(|part| !part.is_empty());
        let Some(server) = parts.next() else {
            return Err(format!("invalid UNC Windows path: {raw}"));
        };
        let Some(share) = parts.next() else {
            return Err(format!("invalid UNC Windows path: {raw}"));
        };
        if matches!(server, "." | "..") || matches!(share, "." | "..") {
            return Err(format!("invalid UNC Windows path: {raw}"));
        }

        let mut components = vec![format!("//{server}/{share}")];
        append_normalized_components(parts, &mut components, raw)?;
        return Ok(Some(components));
    }

    Ok(None)
}

fn append_normalized_components<'a>(
    components: impl IntoIterator<Item = &'a str>,
    normalized: &mut Vec<String>,
    raw: &str,
) -> Result<(), String> {
    for component in components {
        match component {
            "" | "." => {}
            ".." if normalized.len() == 1 => {
                return Err(format!("Windows path escapes its drive/share root: {raw}"));
            }
            ".." => {
                normalized.pop();
            }
            component => normalized.push(component.to_owned()),
        }
    }
    Ok(())
}

fn is_device_path(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    let leading_separators = bytes
        .iter()
        .take_while(|byte| is_separator_byte(**byte))
        .count();
    leading_separators >= 2
        && bytes
            .get(leading_separators)
            .is_some_and(|byte| *byte == b'?' || *byte == b'.')
        && bytes
            .get(leading_separators + 1)
            .is_some_and(|byte| is_separator_byte(*byte))
}

fn contains_parent_component(raw: &str) -> bool {
    raw.split(is_separator).any(|component| component == "..")
}

fn is_separator(character: char) -> bool {
    character == '/' || character == '\\'
}

fn is_separator_byte(byte: u8) -> bool {
    byte == b'/' || byte == b'\\'
}

fn native_path(raw: &str, base: &Path) -> PathBuf {
    if Path::new(raw).is_absolute() {
        return PathBuf::from(raw);
    }

    let mut path = base.to_path_buf();
    for component in raw
        .split(is_separator)
        .filter(|component| !component.is_empty())
    {
        path.push(component);
    }
    path
}

fn introduces_native_path_prefix_or_root(component: &str) -> bool {
    let bytes = component.as_bytes();
    component.starts_with(['/', '\\'])
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

fn matches_components(prefix: &[String], input: &[String]) -> bool {
    prefix.len() <= input.len()
        && prefix
            .iter()
            .zip(input)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

pub(crate) fn config_error(config_file: &Path, message: impl Display) -> String {
    format!("{}: {message}", config_file.display())
}

fn is_valid_property_name(name: &str) -> bool {
    let mut characters = name.chars();
    matches!(
        characters.next(),
        Some(character) if character.is_ascii_alphabetic() || character == '_'
    ) && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

#[cfg(test)]
mod tests {
    use super::fs;
    #[cfg(target_os = "linux")]
    use super::read_override_file_with_budget;
    use std::cell::Cell;

    struct RecordingBudget {
        visits: Cell<usize>,
        bytes: Cell<usize>,
        reservation: Cell<usize>,
    }

    impl crate::ProjectWorkBudget for RecordingBudget {
        fn check_cancelled(&self) -> Result<(), String> {
            Ok(())
        }

        fn charge_path_visits(&self, amount: usize) -> Result<(), String> {
            self.visits.set(self.visits.get() + amount);
            Ok(())
        }

        fn ensure_file_read_fits(&self, max_bytes: usize) -> Result<(), String> {
            self.reservation.set(self.reservation.get() + max_bytes);
            Ok(())
        }

        fn charge_file_bytes(&self, amount: usize) -> Result<(), String> {
            self.bytes.set(self.bytes.get() + amount);
            Ok(())
        }
    }

    #[test]
    fn explicit_budgeted_refresh_observes_same_stamp_override_changes() {
        let root = tempfile::tempdir().expect("temporary workspace");
        let path = root.path().join(super::LOCAL_CONFIG_NAME);
        let initial = "[properties]\nName = 'first'\n";
        let replacement = "[properties]\nName = 'other'\n";
        assert_eq!(initial.len(), replacement.len());
        fs::write(&path, initial).expect("initial override");
        let session = super::OverrideSession::new(None);
        session.capture_workspace(root.path()).expect("capture");
        fs::write(&path, replacement).expect("replace override");
        let budget = RecordingBudget {
            visits: Cell::new(0),
            bytes: Cell::new(0),
            reservation: Cell::new(0),
        };

        session
            .refresh_path_with_budget(&path, &budget)
            .expect("refresh changed override");
        let effective = session
            .effective_for(Some(root.path()), None)
            .expect("effective override");

        assert_eq!(
            effective.properties.get("name").map(String::as_str),
            Some("other")
        );
        assert!(budget.visits.get() > 0, "filesystem probes are charged");
        assert_eq!(budget.bytes.get(), replacement.len());
        assert!(budget.reservation.get() >= replacement.len());
    }

    #[test]
    fn mark_all_dirty_re_reads_every_captured_override_on_next_lookup() {
        let root = tempfile::tempdir().expect("temporary workspace");
        let path = root.path().join(super::LOCAL_CONFIG_NAME);
        fs::write(&path, "[properties]\nName = 'first'\n").expect("initial override");
        let session = super::OverrideSession::new(None);
        session.capture_workspace(root.path()).expect("capture");
        fs::write(&path, "[properties]\nName = 'fresh'\n").expect("updated override");
        let captured = session
            .effective_for(Some(root.path()), None)
            .expect("captured override");
        assert_eq!(
            captured.properties.get("name").map(String::as_str),
            Some("first"),
            "lookups keep the session capture until told otherwise"
        );

        session
            .mark_all_dirty()
            .expect("mark captured overrides dirty");
        let effective = session
            .effective_for(Some(root.path()), None)
            .expect("re-read override");

        assert_eq!(
            effective.properties.get("name").map(String::as_str),
            Some("fresh")
        );
    }

    #[test]
    fn transient_budget_refusal_does_not_poison_the_captured_override() {
        struct RefusingBudget;

        impl crate::ProjectWorkBudget for RefusingBudget {
            fn check_cancelled(&self) -> Result<(), String> {
                Ok(())
            }

            fn charge_path_visits(&self, _amount: usize) -> Result<(), String> {
                Err("workspace notification reconciliation work budget exceeded".to_owned())
            }

            fn ensure_file_read_fits(&self, _max_bytes: usize) -> Result<(), String> {
                Ok(())
            }

            fn charge_file_bytes(&self, _amount: usize) -> Result<(), String> {
                Ok(())
            }

            fn is_transient_error(&self, error: &str) -> bool {
                error == "workspace notification reconciliation work budget exceeded"
            }
        }

        let root = tempfile::tempdir().expect("temporary workspace");
        let path = root.path().join(super::LOCAL_CONFIG_NAME);
        fs::write(&path, "[properties]\nName = 'first'\n").expect("initial override");
        let session = super::OverrideSession::new(None);
        session
            .capture_workspace(root.path())
            .expect("capture initial layer");
        fs::write(&path, "[properties]\nName = 'fresh'\n").expect("updated override");

        let error = session
            .refresh_path_with_budget(&path, &RefusingBudget)
            .expect_err("the event budget must stop this refresh");
        assert_eq!(
            error,
            "workspace notification reconciliation work budget exceeded"
        );

        let effective = session
            .effective_for(Some(root.path()), None)
            .expect("a later bounded capture retries the valid override");
        assert_eq!(
            effective.properties.get("name").map(String::as_str),
            Some("fresh")
        );
    }

    #[test]
    fn cancelled_override_refresh_propagates_cancellation_and_retries_later() {
        struct CancelledBudget;

        impl crate::ProjectWorkBudget for CancelledBudget {
            fn check_cancelled(&self) -> Result<(), String> {
                Err("request cancelled".to_owned())
            }

            fn charge_path_visits(&self, _amount: usize) -> Result<(), String> {
                Ok(())
            }

            fn ensure_file_read_fits(&self, _max_bytes: usize) -> Result<(), String> {
                Ok(())
            }

            fn charge_file_bytes(&self, _amount: usize) -> Result<(), String> {
                Ok(())
            }

            fn is_transient_error(&self, error: &str) -> bool {
                error == "request cancelled"
            }
        }

        let root = tempfile::tempdir().expect("temporary workspace");
        let path = root.path().join(super::LOCAL_CONFIG_NAME);
        fs::write(&path, "[properties]\nName = 'first'\n").expect("initial override");
        let session = super::OverrideSession::new(None);
        session
            .capture_workspace(root.path())
            .expect("capture initial layer");
        fs::write(&path, "[properties]\nName = 'fresh'\n").expect("updated override");

        assert_eq!(
            session.refresh_path_with_budget(&path, &CancelledBudget),
            Err("request cancelled".to_owned()),
            "refresh must propagate rather than swallow cancellation"
        );
        let effective = session
            .effective_for(Some(root.path()), None)
            .expect("later request retries the cancelled refresh");
        assert_eq!(
            effective.properties.get("name").map(String::as_str),
            Some("fresh")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fifo_substitution_after_candidate_inspection_is_rejected_without_blocking() {
        let root = tempfile::tempdir().expect("temporary configuration directory");
        let path = root.path().join("config.toml");
        fs::write(&path, "[properties]\nName = 'value'\n").expect("configuration file");
        let _hook = super::install_fifo_substitution_hook(&path);
        let unrelated_path = root.path().join("unrelated.toml");
        fs::write(&unrelated_path, "[properties]\nName = 'unrelated'\n")
            .expect("unrelated configuration file");
        assert!(read_override_file_with_budget(&unrelated_path, None).is_ok());

        let error = read_override_file_with_budget(&path, None)
            .expect_err("substituted FIFO must not be read");
        assert!(
            error.contains("could not open") || error.contains("not a regular file"),
            "unexpected FIFO substitution error: {error}"
        );
    }
}

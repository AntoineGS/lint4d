use std::collections::{BTreeMap, BTreeSet};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPath {
    pub path: PathBuf,
    pub mapping: Option<PathMapping>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverrideLayer {
    properties: BTreeMap<String, String>,
    path_mappings: Vec<PathMapping>,
    config_file: PathBuf,
}

type CapturedLayer = Result<Option<OverrideLayer>, String>;

#[derive(Debug, Clone)]
pub struct OverrideSession {
    user_config_file: Option<PathBuf>,
    captured: Arc<Mutex<BTreeMap<PathBuf, CapturedLayer>>>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOverrideFile {
    #[serde(default)]
    properties: BTreeMap<String, String>,
    #[serde(default)]
    path_mappings: Vec<RawPathMapping>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPathMapping {
    from: String,
    to: String,
}

impl OverrideLayer {
    pub fn parse(text: &str, config_file: &Path) -> Result<Self, String> {
        let raw: RawOverrideFile = toml::from_str(text)
            .map_err(|error| config_error(config_file, format_args!("failed to parse: {error}")))?;

        let mut properties = BTreeMap::new();
        for (name, value) in raw.properties {
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

        let mut path_mappings = Vec::with_capacity(raw.path_mappings.len());
        let mut mapping_prefixes = BTreeSet::new();
        for mapping in raw.path_mappings {
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
                Ok(Some(layer)) => layers.push(layer),
                Ok(None) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(EffectiveOverrides::merge(&layers))
    }

    fn capture_path(&self, path: &Path) -> Result<(), String> {
        let path = normalize_absolute_lexical(path)?;
        let mut captured = self.captured.lock().map_err(|_| capture_store_poisoned())?;
        if let Some(result) = captured.get(&path) {
            return match result {
                Ok(_) => Ok(()),
                Err(error) => Err(error.clone()),
            };
        }

        let result = read_override_file(&path);
        captured.insert(path, result.clone());
        result.map(|_| ())
    }

    fn captured_layer(&self, path: &Path) -> Result<CapturedLayer, String> {
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

fn read_override_file(path: &Path) -> CapturedLayer {
    let Some(_) = inspect_candidate(path)? else {
        return Ok(None);
    };

    #[cfg(all(test, target_os = "linux"))]
    maybe_substitute_candidate_after_inspection(path);

    let file = open_candidate(path)
        .map_err(|error| format!("could not open {}: {error}", path.display()))?;
    let opened_metadata = file
        .metadata()
        .map_err(|error| format!("could not inspect opened {}: {error}", path.display()))?;
    validate_regular_file(path, &opened_metadata)?;

    let mut bytes = Vec::new();
    file.take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(format!(
            "{} exceeds {MAX_CONFIG_BYTES} bytes",
            path.display()
        ));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|error| format!("invalid UTF-8 in {}: {error}", path.display()))?;
    OverrideLayer::parse(text, path).map(Some)
}

fn inspect_candidate(path: &Path) -> Result<Option<fs::Metadata>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not inspect {}: {error}", path.display())),
    };
    let metadata = if metadata.file_type().is_symlink() {
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
    let should_substitute = FIFO_SUBSTITUTION_HOOK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("FIFO substitution hook lock")
        .take()
        .is_some_and(|expected| expected == path);
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

fn normalize_absolute_lexical(path: &Path) -> Result<PathBuf, String> {
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

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
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

fn config_error(config_file: &Path, message: impl Display) -> String {
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
    use super::{fs, read_override_file};

    #[cfg(target_os = "linux")]
    #[test]
    fn fifo_substitution_after_candidate_inspection_is_rejected_without_blocking() {
        let root = tempfile::tempdir().expect("temporary configuration directory");
        let path = root.path().join("config.toml");
        fs::write(&path, "[properties]\nName = 'value'\n").expect("configuration file");
        let _hook = super::install_fifo_substitution_hook(&path);

        let error = read_override_file(&path).expect_err("substituted FIFO must not be read");
        assert!(
            error.contains("could not open") || error.contains("not a regular file"),
            "unexpected FIFO substitution error: {error}"
        );
    }
}

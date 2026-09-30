use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use crate::build_selection::BuildChoice;
use crate::conditional::ConditionalContext;
use crate::delphi_overrides::{EffectiveOverrides, OverrideLayer, RawPathMapping, config_error};

#[derive(Debug, Clone)]
pub(crate) struct ConfigurationLayer {
    pub(crate) shared: OverrideLayer,
    pub(crate) installations: BTreeMap<String, OverrideLayer>,
    pub(crate) rtl_constants: BTreeMap<String, Vec<String>>,
    projects: HashMap<PathBuf, String>,
    build_projects: HashMap<PathBuf, BuildChoice>,
    pub(crate) source_stamp: ConfigurationSourceStamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigurationSourceStamp {
    pub path: PathBuf,
    pub byte_len: Option<u64>,
    pub content_hash: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfiguration {
    #[serde(default)]
    properties: BTreeMap<String, String>,
    #[serde(default)]
    path_mappings: Vec<RawPathMapping>,
    #[serde(default)]
    installations: BTreeMap<String, RawInstallation>,
    #[serde(default)]
    projects: BTreeMap<String, RawProject>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInstallation {
    #[serde(default)]
    properties: BTreeMap<String, String>,
    #[serde(default)]
    path_mappings: Vec<RawPathMapping>,
    #[serde(default, rename = "rtlVersionConstants")]
    rtl_version_constants: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProject {
    #[serde(default)]
    installation: Option<String>,
    #[serde(default)]
    config: Option<String>,
    #[serde(default)]
    platform: Option<String>,
}

impl ConfigurationLayer {
    pub(crate) fn parse(text: &str, config_file: &Path) -> Result<Self, String> {
        let raw: RawConfiguration = toml::from_str(text)
            .map_err(|error| config_error(config_file, format_args!("failed to parse: {error}")))?;
        let shared = OverrideLayer::from_parts(raw.properties, raw.path_mappings, config_file)?;

        let mut installations = BTreeMap::new();
        let mut rtl_constants = BTreeMap::new();
        let mut installation_ids = HashSet::new();
        for (id, profile) in raw.installations {
            let canonical_id = id.to_ascii_lowercase();
            if id.trim().is_empty() || !installation_ids.insert(canonical_id.clone()) {
                return Err(config_error(
                    config_file,
                    format_args!("empty or duplicate installation ID `{id}`"),
                ));
            }
            if let Some(names) = &profile.rtl_version_constants {
                for name in names {
                    if !ConditionalContext::is_rtl_version_constant(name) {
                        return Err(config_error(
                            config_file,
                            format_args!(
                                "invalid rtlVersionConstants entry `{name}` for installation `{id}`"
                            ),
                        ));
                    }
                }
                rtl_constants.insert(canonical_id.clone(), names.clone());
            }
            installations.insert(
                id,
                OverrideLayer::from_parts(profile.properties, profile.path_mappings, config_file)?,
            );
        }

        let mut projects = HashMap::new();
        let mut build_projects = HashMap::new();
        let mut project_selectors = HashSet::new();
        for (selector, project) in raw.projects {
            if project.installation.is_none()
                && project.config.is_none()
                && project.platform.is_none()
            {
                return Err(config_error(
                    config_file,
                    format_args!(
                        "project selector `{selector}` sets no installation, config, or platform"
                    ),
                ));
            }
            if project
                .installation
                .as_ref()
                .is_some_and(|value| value.trim().is_empty())
            {
                return Err(config_error(
                    config_file,
                    format_args!("project selector `{selector}` has an empty installation ID"),
                ));
            }
            for (name, value) in [("config", &project.config), ("platform", &project.platform)] {
                if value.as_ref().is_some_and(|value| value.trim().is_empty()) {
                    return Err(config_error(
                        config_file,
                        format_args!("project selector `{selector}` has an empty {name}"),
                    ));
                }
            }
            let key = normalize_selector(config_file, &selector)?;
            if !project_selectors.insert(key.clone()) {
                return Err(config_error(
                    config_file,
                    format_args!("duplicate normalized project selector `{selector}`"),
                ));
            }
            if let Some(installation) = project.installation {
                projects.insert(key.clone(), installation);
            }
            let build_choice = BuildChoice {
                config: project.config,
                platform: project.platform,
            };
            if build_choice.config.is_some() || build_choice.platform.is_some() {
                build_projects.insert(key, build_choice);
            }
        }

        Ok(Self {
            shared,
            installations,
            rtl_constants,
            projects,
            build_projects,
            source_stamp: ConfigurationSourceStamp {
                path: config_file.to_path_buf(),
                byte_len: Some(text.len() as u64),
                content_hash: Some(crate::content_hash_bytes(text.as_bytes())),
            },
        })
    }
}

/// The immutable installation-related view of the exact captured layers for a
/// project. The shared/profile ordering remains available until a profile is
/// selected so a more-local shared value may supersede an earlier profile.
#[derive(Debug, Clone)]
pub struct ProjectConfiguration {
    layers: Vec<ConfigurationLayer>,
    selectors: HashMap<PathBuf, String>,
    build_selectors: HashMap<PathBuf, BuildChoice>,
    source_stamps: Vec<ConfigurationSourceStamp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedInstallation {
    pub id: String,
    pub overrides: EffectiveOverrides,
    pub rtl_version_constants: Option<Vec<String>>,
}

impl ProjectConfiguration {
    pub(crate) fn from_layers(layers: Vec<ConfigurationLayer>) -> Self {
        let mut selectors = HashMap::new();
        let mut build_selectors = HashMap::new();
        for layer in &layers {
            selectors.extend(layer.projects.clone());
            build_selectors.extend(layer.build_projects.clone());
        }
        let source_stamps = layers
            .iter()
            .map(|layer| layer.source_stamp.clone())
            .collect();
        Self {
            layers,
            selectors,
            build_selectors,
            source_stamps,
        }
    }

    fn from_layers_and_source_stamps(
        layers: Vec<ConfigurationLayer>,
        source_stamps: Vec<ConfigurationSourceStamp>,
    ) -> Self {
        let mut configuration = Self::from_layers(layers);
        configuration.source_stamps = source_stamps;
        configuration
    }

    pub fn source_stamps(&self) -> &[ConfigurationSourceStamp] {
        &self.source_stamps
    }

    pub fn installation_ids(&self) -> Vec<String> {
        let mut ids = BTreeMap::<String, String>::new();
        for layer in &self.layers {
            for id in layer.installations.keys() {
                ids.entry(id.to_ascii_lowercase())
                    .or_insert_with(|| id.clone());
            }
        }
        ids.into_values().collect()
    }

    pub fn profile(&self, id: &str) -> Result<ResolvedInstallation, String> {
        let Some(canonical) = self
            .installation_ids()
            .into_iter()
            .find(|candidate| candidate.eq_ignore_ascii_case(id))
        else {
            return Err(format!("unknown Delphi installation `{id}`"));
        };
        let mut applicable = Vec::new();
        let mut rtl_version_constants = None;
        for layer in &self.layers {
            applicable.push(layer.shared.clone());
            if let Some(names) = layer.rtl_constants.get(&canonical.to_ascii_lowercase()) {
                rtl_version_constants = Some(names.clone());
            }
            if let Some(profile) = layer
                .installations
                .iter()
                .find(|(candidate, _)| candidate.eq_ignore_ascii_case(&canonical))
                .map(|(_, profile)| profile)
            {
                applicable.push(profile.clone());
            }
        }
        Ok(ResolvedInstallation {
            id: canonical,
            overrides: EffectiveOverrides::merge(&applicable),
            rtl_version_constants,
        })
    }

    pub fn configured_installation_for(&self, project: &Path) -> Option<&str> {
        let project = absolute_lexical(project).ok()?;
        let key = canonical_path(&project);
        self.selectors.get(&key).map(String::as_str)
    }

    pub fn configured_build_for(&self, project: &Path) -> BuildChoice {
        let Ok(project) = absolute_lexical(project) else {
            return BuildChoice::default();
        };
        let key = canonical_path(&project);
        self.build_selectors.get(&key).cloned().unwrap_or_default()
    }
}

impl crate::delphi_overrides::OverrideSession {
    pub fn configuration_source_stamps_for(
        &self,
        workspace_root: Option<&Path>,
        project_file: Option<&Path>,
        budget: Option<&dyn crate::ProjectWorkBudget>,
    ) -> Result<Vec<ConfigurationSourceStamp>, String> {
        let mut paths = Vec::with_capacity(3);
        if let Some(user_config_file) = self.user_config_file.as_ref() {
            super::delphi_overrides::push_unique_path(&mut paths, user_config_file.clone());
        }
        if let Some(workspace_root) = workspace_root {
            let root = super::delphi_overrides::normalize_absolute_lexical(workspace_root)?;
            super::delphi_overrides::push_unique_path(
                &mut paths,
                root.join(super::delphi_overrides::LOCAL_CONFIG_NAME),
            );
        }
        if let Some(project_file) = project_file {
            let project_file = super::delphi_overrides::normalize_absolute_lexical(project_file)?;
            let directory = project_file.parent().unwrap_or(&project_file);
            super::delphi_overrides::push_unique_path(
                &mut paths,
                directory.join(super::delphi_overrides::LOCAL_CONFIG_NAME),
            );
        }

        let mut stamps = Vec::with_capacity(paths.len());
        for path in paths {
            if let Some(budget) = budget {
                budget.check_cancelled()?;
                budget.charge_path_visits(1)?;
            }
            match super::delphi_overrides::override_source_stamp_with_budget(&path, budget)? {
                Some(stamp) => stamps.push(stamp),
                None => stamps.push(ConfigurationSourceStamp {
                    path,
                    byte_len: None,
                    content_hash: None,
                }),
            }
        }
        Ok(stamps)
    }

    pub fn configuration_source_stamps_are_current(
        &self,
        expected_stamps: &[ConfigurationSourceStamp],
        budget: Option<&dyn crate::ProjectWorkBudget>,
    ) -> Result<bool, String> {
        for expected in expected_stamps {
            if let Some(budget) = budget {
                budget.check_cancelled()?;
                budget.charge_path_visits(1)?;
            }
            let current = match super::delphi_overrides::override_source_stamp_with_budget(
                &expected.path,
                budget,
            ) {
                Ok(Some(stamp)) => stamp,
                Ok(None) => ConfigurationSourceStamp {
                    path: expected.path.clone(),
                    byte_len: None,
                    content_hash: None,
                },
                Err(_) => return Ok(false),
            };
            if &current != expected {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn configuration_for(
        &self,
        workspace_root: Option<&Path>,
        project_file: Option<&Path>,
    ) -> Result<ProjectConfiguration, String> {
        self.configuration_for_with_work_budget(workspace_root, project_file, None)
    }

    pub fn configuration_for_with_work_budget(
        &self,
        workspace_root: Option<&Path>,
        project_file: Option<&Path>,
        budget: Option<&dyn crate::ProjectWorkBudget>,
    ) -> Result<ProjectConfiguration, String> {
        let mut paths = Vec::with_capacity(3);
        if let Some(user_config_file) = self.user_config_file.as_ref() {
            super::delphi_overrides::push_unique_path(&mut paths, user_config_file.clone());
        }
        if let Some(workspace_root) = workspace_root {
            let root = super::delphi_overrides::normalize_absolute_lexical(workspace_root)?;
            super::delphi_overrides::push_unique_path(
                &mut paths,
                root.join(super::delphi_overrides::LOCAL_CONFIG_NAME),
            );
        }
        if let Some(project_file) = project_file {
            let project_file = super::delphi_overrides::normalize_absolute_lexical(project_file)?;
            let directory = project_file.parent().unwrap_or(&project_file);
            super::delphi_overrides::push_unique_path(
                &mut paths,
                directory.join(super::delphi_overrides::LOCAL_CONFIG_NAME),
            );
        }

        let mut layers = Vec::new();
        let mut source_stamps = Vec::with_capacity(paths.len());
        for path in paths {
            if let Some(budget) = budget {
                budget.check_cancelled()?;
                budget.charge_path_visits(1)?;
            }
            self.capture_path_with_work_budget(&path, budget)?;
            match self.captured_layer(&path)? {
                Ok(Some(layer)) => {
                    source_stamps.push(layer.source_stamp.clone());
                    layers.push(layer);
                }
                Ok(None) => source_stamps.push(ConfigurationSourceStamp {
                    path,
                    byte_len: None,
                    content_hash: None,
                }),
                Err(error) => return Err(error),
            }
        }
        Ok(ProjectConfiguration::from_layers_and_source_stamps(
            layers,
            source_stamps,
        ))
    }

    pub fn configuration_sources_are_current(
        &self,
        configuration: &ProjectConfiguration,
        budget: Option<&dyn crate::ProjectWorkBudget>,
    ) -> Result<bool, String> {
        for expected in configuration.source_stamps() {
            if let Some(budget) = budget {
                budget.check_cancelled()?;
                budget.charge_path_visits(1)?;
            }
            let current = match super::delphi_overrides::read_override_file_with_budget(
                &expected.path,
                budget,
            ) {
                Ok(Some(layer)) => layer.source_stamp,
                Ok(None) => ConfigurationSourceStamp {
                    path: expected.path.clone(),
                    byte_len: None,
                    content_hash: None,
                },
                Err(_) => return Ok(false),
            };
            if &current != expected {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

fn normalize_selector(config_file: &Path, selector: &str) -> Result<PathBuf, String> {
    if selector.trim().is_empty() {
        return Err(config_error(
            config_file,
            "project selector may not be empty",
        ));
    }
    let path = Path::new(selector);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        config_file
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(path)
    };
    let normalized =
        absolute_lexical(&resolved).map_err(|error| config_error(config_file, error))?;
    Ok(canonical_path(&normalized))
}

fn absolute_lexical(path: &Path) -> Result<PathBuf, String> {
    let absolute = std::path::absolute(path)
        .map_err(|error| format!("could not resolve {}: {error}", path.display()))?;
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

fn canonical_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let mut canonical = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Prefix(prefix) => canonical.push(fold_windows_ascii(prefix.as_os_str())),
                Component::RootDir => canonical.push(component.as_os_str()),
                Component::CurDir => {}
                Component::ParentDir => {
                    canonical.pop();
                }
                Component::Normal(part) => canonical.push(fold_windows_ascii(part)),
            }
        }
        canonical
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

#[cfg(windows)]
fn fold_windows_ascii(value: &std::ffi::OsStr) -> std::ffi::OsString {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    std::ffi::OsString::from_wide(
        &value
            .encode_wide()
            .map(|unit| {
                if (b'A' as u16..=b'Z' as u16).contains(&unit) {
                    unit + (b'a' - b'A') as u16
                } else {
                    unit
                }
            })
            .collect::<Vec<_>>(),
    )
}

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use crate::delphi_overrides::{EffectiveOverrides, OverrideLayer, RawPathMapping, config_error};

#[derive(Debug, Clone)]
pub(crate) struct ConfigurationLayer {
    pub(crate) shared: OverrideLayer,
    pub(crate) installations: BTreeMap<String, OverrideLayer>,
    projects: HashMap<String, String>,
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
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProject {
    installation: String,
}

impl ConfigurationLayer {
    pub(crate) fn parse(text: &str, config_file: &Path) -> Result<Self, String> {
        let raw: RawConfiguration = toml::from_str(text)
            .map_err(|error| config_error(config_file, format_args!("failed to parse: {error}")))?;
        let shared = OverrideLayer::from_parts(raw.properties, raw.path_mappings, config_file)?;

        let mut installations = BTreeMap::new();
        let mut installation_ids = HashSet::new();
        for (id, profile) in raw.installations {
            let canonical_id = id.to_ascii_lowercase();
            if id.trim().is_empty() || !installation_ids.insert(canonical_id) {
                return Err(config_error(
                    config_file,
                    format_args!("empty or duplicate installation ID `{id}`"),
                ));
            }
            installations.insert(
                id,
                OverrideLayer::from_parts(profile.properties, profile.path_mappings, config_file)?,
            );
        }

        let mut projects = HashMap::new();
        for (selector, project) in raw.projects {
            let key = normalize_selector(config_file, &selector)?;
            if projects.insert(key, project.installation).is_some() {
                return Err(config_error(
                    config_file,
                    format_args!("duplicate normalized project selector `{selector}`"),
                ));
            }
        }

        Ok(Self {
            shared,
            installations,
            projects,
        })
    }
}

/// The immutable installation-related view of the exact captured layers for a
/// project. The shared/profile ordering remains available until a profile is
/// selected so a more-local shared value may supersede an earlier profile.
#[derive(Debug, Clone)]
pub struct ProjectConfiguration {
    layers: Vec<ConfigurationLayer>,
    selectors: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedInstallation {
    pub id: String,
    pub overrides: EffectiveOverrides,
}

impl ProjectConfiguration {
    pub(crate) fn from_layers(layers: Vec<ConfigurationLayer>) -> Self {
        let mut selectors = HashMap::new();
        for layer in &layers {
            selectors.extend(layer.projects.clone());
        }
        Self { layers, selectors }
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
        for layer in &self.layers {
            applicable.push(layer.shared.clone());
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
        })
    }

    pub fn configured_installation_for(&self, project: &Path) -> Option<&str> {
        let project = absolute_lexical(project).ok()?;
        let key = canonical_path(&project);
        self.selectors.get(&key).map(String::as_str)
    }
}

impl crate::delphi_overrides::OverrideSession {
    pub fn configuration_for(
        &self,
        workspace_root: Option<&Path>,
        project_file: Option<&Path>,
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
        for path in paths {
            self.capture_path(&path)?;
            match self.captured_layer(&path)? {
                Ok(Some(layer)) => layers.push(layer),
                Ok(None) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(ProjectConfiguration::from_layers(layers))
    }
}

fn normalize_selector(config_file: &Path, selector: &str) -> Result<String, String> {
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

fn canonical_path(path: &Path) -> String {
    let value = path.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    {
        value.to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        value
    }
}

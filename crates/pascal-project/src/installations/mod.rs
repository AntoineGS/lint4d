use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::delphi_overrides::PathMapping;
use crate::{MetadataObservation, ProjectPathIssue};

pub(crate) type PropertyMap = BTreeMap<String, String>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelocatedEnvironment {
    pub properties: PropertyMap,
    pub inferred_mappings: Vec<PathMapping>,
    pub read_roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct InstallationEnvironment {
    pub properties: PropertyMap,
    pub read_roots: Vec<PathBuf>,
    pub inferred_mappings: Vec<PathMapping>,
    pub metadata_files: Vec<PathBuf>,
    pub metadata_observations: Vec<MetadataObservation>,
    pub path_issues: Vec<ProjectPathIssue>,
    pub warnings: Vec<String>,
}

mod ide_paths;
mod roots;
mod rsvars;

#[allow(unused_imports)]
pub(crate) use ide_paths::{IdePaths, evaluate_ide_paths, load_installation};
#[allow(unused_imports)]
pub(crate) use roots::{relocate_environment, resolve_path_with_inferred};
#[allow(unused_imports)]
pub(crate) use rsvars::parse_rsvars;

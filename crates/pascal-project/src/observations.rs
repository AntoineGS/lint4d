use super::*;

/// What reading a project produced besides its analysis inputs: the files
/// consulted, how they were authorized, warnings, and System.pas lookups.
/// Owners update it while resolving; it never affects context equality.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectObservations {
    /// Project, main-source, imported option-set, and automatic-selection
    /// candidate files consulted. Consumers revalidate these paths without
    /// rediscovering or reparsing unrelated source files.
    pub metadata_files: Vec<PathBuf>,
    /// The authorization provenance for each metadata observation. A path in
    /// `metadata_files` without a payload observation is stat-only.
    pub metadata_observations: Vec<MetadataObservation>,
    pub warnings: Vec<String>,
    /// Ordered System.pas directory lookups used to derive RTL constants.
    pub system_pas_searches: Vec<SystemPasSearchObservation>,
}

impl ProjectObservations {
    pub fn visit_recovery_payload(
        &self,
        visit: &mut dyn FnMut(usize) -> Result<(), String>,
    ) -> Result<(), String> {
        for path in &self.metadata_files {
            visit(path.as_os_str().len())?;
        }
        for search in &self.system_pas_searches {
            visit(search.directory.as_os_str().len())?;
            if let Some(path) = &search.selected_path {
                visit(path.as_os_str().len())?;
            }
        }
        for observation in &self.metadata_observations {
            observation.visit_recovery_payload(visit)?;
        }
        for warning in &self.warnings {
            visit(warning.len())?;
        }
        Ok(())
    }
}

/// A discovered context and the observations made while constructing it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscoveredProject {
    pub context: ProjectContext,
    pub observations: ProjectObservations,
}

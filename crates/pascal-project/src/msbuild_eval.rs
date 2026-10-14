use super::assembly::{
    add_metadata_file, combine_path_provenance, expanded_path_provenance, full_range,
    inherit_path_provenance, property_list_item, trim_provenance_ranges, trim_ranges,
};
use super::authorization::{
    ProvenanceRange, extension_is, is_compiled_reference, lexical_normalize, missing_path_warning,
    project_path_candidate, resolve_existing_path_status_with_provenance,
};
use super::*;

#[derive(Debug)]
pub(super) struct ProjectBuilder {
    pub(super) properties: HashMap<String, String>,
    pub(super) configured_ranges: HashMap<String, Vec<Range<usize>>>,
    pub(super) property_provenance_ranges: HashMap<String, Vec<ProvenanceRange>>,
    pub(super) property_default_provenances: HashMap<String, ProjectPathProvenance>,
    pub(super) configured_properties: HashSet<String>,
    global_properties: HashSet<String>,
    pub(super) overrides: EffectiveOverrides,
    pub(super) unknown_properties: HashSet<String>,
    pub(super) unknown_import_taint: bool,
    installation_data_mode: bool,
    pub(super) references: Vec<DccReference>,
    pub(super) path_issues: Vec<ProjectPathIssue>,
    pub(super) warnings: Vec<String>,
    pub(super) incomplete: bool,
    active_imports: HashSet<PathBuf>,
    import_count: usize,
    pub(super) project_dir: PathBuf,
    property_bytes: usize,
    pub(super) metadata_files: Vec<PathBuf>,
    pub(super) metadata_observations: Vec<MetadataObservation>,
    pub(super) system_pas_searches: Vec<SystemPasSearchObservation>,
    pub(super) read_policy: ReadPolicy,
}

impl ProjectBuilder {
    pub(super) fn new(
        options: &ProjectOptions,
        overrides: &EffectiveOverrides,
        mut warnings: Vec<String>,
        project_dir: PathBuf,
        read_policy: ReadPolicy,
    ) -> Self {
        let mut properties: HashMap<String, String> =
            overrides.properties.clone().into_iter().collect();
        let mut configured_ranges = properties
            .iter()
            .map(|(name, value)| (name.clone(), full_range(value)))
            .collect::<HashMap<_, _>>();
        let mut configured_properties: HashSet<String> =
            overrides.properties.keys().cloned().collect();
        let mut client_properties = HashSet::new();
        for (name, value) in [
            ("config", options.build_config.as_ref()),
            ("platform", options.platform.as_ref()),
        ] {
            if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
                properties.insert(name.to_string(), value.clone());
                // Client selections retain the legacy project-evaluation
                // source policy even when they replace an override-file
                // value; they still remain immutable global properties.
                configured_properties.remove(name);
                configured_ranges.remove(name);
                client_properties.insert(name.to_string());
            }
        }
        let global_properties: HashSet<String> = properties.keys().cloned().collect();
        properties.entry("config".to_string()).or_default();
        properties.entry("platform".to_string()).or_default();
        let mut property_provenance_ranges = HashMap::new();
        let mut property_default_provenances = HashMap::new();
        for (name, value) in &properties {
            let key = name.to_ascii_lowercase();
            let provenance = if client_properties.contains(&key) {
                ProjectPathProvenance::LegacyNative
            } else if overrides.properties.contains_key(name) {
                ProjectPathProvenance::Configured
            } else {
                ProjectPathProvenance::LegacyNative
            };
            property_default_provenances.insert(key.clone(), provenance.clone());
            if !value.is_empty() {
                property_provenance_ranges.insert(
                    key,
                    vec![ProvenanceRange {
                        range: 0..value.len(),
                        provenance,
                    }],
                );
            }
        }
        let property_bytes = properties.values().map(String::len).sum();
        let mut incomplete = false;
        if property_bytes > MAX_TOTAL_PROPERTY_BYTES {
            let mut provenances = Vec::new();
            for name in &global_properties {
                let provenance = if client_properties.contains(name) {
                    "client initialization options".to_string()
                } else {
                    overrides.property_origins.get(name).map_or_else(
                        || "explicit override session".to_string(),
                        |path| path.display().to_string(),
                    )
                };
                if !provenances.iter().any(|existing| existing == &provenance) {
                    provenances.push(provenance);
                }
            }
            provenances.sort_unstable();
            incomplete = true;
            warnings.push(format!(
                "configured Delphi override properties exceed the {MAX_TOTAL_PROPERTY_BYTES} byte evaluator budget (sources: {})",
                provenances.join(", ")
            ));
        }
        for (name, value) in &properties {
            if value.len() <= MAX_EXPANDED_VALUE_BYTES {
                continue;
            }
            incomplete = true;
            let provenance = if client_properties.contains(name) {
                "client initialization options".to_string()
            } else {
                overrides.property_origins.get(name).map_or_else(
                    || "explicit override session".to_string(),
                    |path| path.display().to_string(),
                )
            };
            warnings.push(format!(
                "configured Delphi property {name} from {provenance} exceeds the {MAX_EXPANDED_VALUE_BYTES} byte per-value evaluator budget"
            ));
        }
        Self {
            properties,
            configured_ranges,
            property_provenance_ranges,
            property_default_provenances,
            configured_properties,
            global_properties,
            overrides: overrides.clone(),
            unknown_properties: HashSet::new(),
            unknown_import_taint: false,
            installation_data_mode: false,
            references: Vec::new(),
            path_issues: Vec::new(),
            warnings,
            incomplete,
            active_imports: HashSet::new(),
            import_count: 0,
            project_dir,
            property_bytes,
            metadata_files: Vec::new(),
            metadata_observations: Vec::new(),
            system_pas_searches: Vec::new(),
            read_policy,
        }
    }

    fn record_payload_observation(&mut self, observation: MetadataObservation) {
        add_metadata_observation(&mut self.metadata_observations, observation);
    }

    pub(super) fn seed_installation_properties(&mut self, properties: &BTreeMap<String, String>) {
        for (name, value) in properties {
            let key = name.to_ascii_lowercase();
            if self.global_properties.contains(&key) {
                continue;
            }
            if let Some(previous) = self.properties.insert(key.clone(), value.clone()) {
                self.property_bytes = self.property_bytes.saturating_sub(previous.len());
            }
            self.property_bytes = self.property_bytes.saturating_add(value.len());
            self.configured_ranges.remove(&key);
            self.configured_properties.remove(&key);
            self.property_provenance_ranges.insert(
                key.clone(),
                vec![ProvenanceRange {
                    range: 0..value.len(),
                    provenance: ProjectPathProvenance::LegacyNative,
                }],
            );
            self.property_default_provenances
                .insert(key, ProjectPathProvenance::LegacyNative);
        }
    }

    pub(super) fn process_installation_file(
        &mut self,
        contents: &str,
        path: &Path,
        tracker: &mut ProjectReadTracker<'_>,
        provenance: &ProjectPathProvenance,
    ) -> Result<(), String> {
        let operations = parse_xml_operations(contents, path)
            .map_err(|error| format!("malformed installation XML: {error}"))?;
        let previous_mode = std::mem::replace(&mut self.installation_data_mode, true);
        let result = self.process_operations(operations, path, tracker, provenance);
        self.installation_data_mode = previous_mode;
        result
    }

    pub(super) fn property(&self, name: &str) -> Option<String> {
        self.properties.get(&name.to_ascii_lowercase()).cloned()
    }

    pub(super) fn property_is_configured(&self, name: &str) -> bool {
        self.configured_properties
            .contains(&name.to_ascii_lowercase())
    }

    pub(super) fn property_provenance(&self, name: &str) -> ProjectPathProvenance {
        let key = name.to_ascii_lowercase();
        self.property_provenance_ranges
            .get(&key)
            .into_iter()
            .flatten()
            .fold(
                self.property_default_provenances
                    .get(&key)
                    .cloned()
                    .unwrap_or(ProjectPathProvenance::LegacyNative),
                |current, range| combine_path_provenance(&current, &range.provenance),
            )
    }

    pub(super) fn property_list_with_provenance(
        &self,
        name: &str,
    ) -> Vec<(String, ProjectPathProvenance)> {
        let Some(value) = self.property(name) else {
            return Vec::new();
        };
        let provenance_ranges = self
            .property_provenance_ranges
            .get(&name.to_ascii_lowercase())
            .map(Vec::as_slice)
            .unwrap_or_default();
        let default_provenance = self
            .property_default_provenances
            .get(&name.to_ascii_lowercase())
            .cloned()
            .unwrap_or(ProjectPathProvenance::LegacyNative);
        let mut result = Vec::new();
        let mut start = 0;
        for separator in value.match_indices(';').map(|(index, _)| index) {
            result.push(property_list_item(
                &value,
                start,
                separator,
                provenance_ranges,
                &default_provenance,
            ));
            start = separator + 1;
        }
        result.push(property_list_item(
            &value,
            start,
            value.len(),
            provenance_ranges,
            &default_provenance,
        ));
        result
    }

    pub(super) fn process_root_dproj(
        &mut self,
        path: &Path,
        tracker: &mut ProjectReadTracker<'_>,
    ) -> Result<(), String> {
        self.metadata_files.push(path.to_path_buf());
        let entry = ProjectPathEntry::legacy(path.to_path_buf());
        let (contents, observation) =
            read_payload_with_tracker(&self.read_policy, &entry, MAX_PROJECT_BYTES, tracker)
                .map_err(|error| format!("could not read project {}: {error}", path.display()))?;
        self.record_payload_observation(observation);
        let operations = parse_xml_operations(&contents, path)?;
        // MSBuild's project identity belongs to the root project, not to an
        // imported option set. Keep explicit globals at their existing priority.
        let key = "msbuildprojectname";
        if !self.global_properties.contains(key)
            && let Some(name) = path.file_stem()
        {
            self.set_property(
                key,
                name.to_string_lossy().into_owned(),
                false,
                Vec::new(),
                false,
                Vec::new(),
                &entry.provenance,
                path,
            );
            self.global_properties.insert(key.to_owned());
        }
        self.process_operations(operations, path, tracker, &entry.provenance)?;
        Ok(())
    }

    pub(super) fn process_operations(
        &mut self,
        operations: Vec<XmlOperation>,
        source_file: &Path,
        tracker: &mut ProjectReadTracker<'_>,
        source_provenance: &ProjectPathProvenance,
    ) -> Result<(), String> {
        let base = source_file.parent().unwrap_or_else(|| Path::new("."));
        for operation in operations {
            match operation {
                XmlOperation::PropertyGroup(group) => {
                    self.process_property_group(group, source_file, base, source_provenance);
                }
                XmlOperation::DccReference(reference) => {
                    if is_compiled_reference(&reference.include) {
                        continue;
                    }
                    let condition = condition_matches(
                        reference.condition.as_deref(),
                        ConditionEnvironment {
                            properties: &self.properties,
                            unknown_properties: &self.unknown_properties,
                            unknown_import_taint: self.unknown_import_taint,
                            overrides: &self.overrides,
                            read_policy: &self.read_policy,
                        },
                        base,
                        &mut self.metadata_files,
                        &mut self.warnings,
                        source_file,
                    );
                    match condition {
                        TruthValue::True => self.references.push(DccReference {
                            include: reference.include,
                            condition: None,
                            source_file: source_file.to_path_buf(),
                            source_provenance: source_provenance.clone(),
                        }),
                        TruthValue::False => {}
                        TruthValue::Unknown => self.incomplete = true,
                    }
                }
                XmlOperation::Import(import) => {
                    self.process_import(import, source_file, base, tracker, source_provenance)?;
                }
                XmlOperation::Unsupported(message) => self.warnings.push(message),
            }
        }
        Ok(())
    }

    fn process_property_group(
        &mut self,
        group: PropertyGroup,
        source_file: &Path,
        base: &Path,
        source_provenance: &ProjectPathProvenance,
    ) {
        let group_result = condition_matches(
            group.condition.as_deref(),
            ConditionEnvironment {
                properties: &self.properties,
                unknown_properties: &self.unknown_properties,
                unknown_import_taint: self.unknown_import_taint,
                overrides: &self.overrides,
                read_policy: &self.read_policy,
            },
            base,
            &mut self.metadata_files,
            &mut self.warnings,
            source_file,
        );
        match group_result {
            TruthValue::False => return,
            TruthValue::Unknown => {
                self.incomplete = true;
                for property in group.properties {
                    self.mark_property_unknown(&property.name, source_file, source_provenance);
                }
                return;
            }
            TruthValue::True => {}
        }
        for property in group.properties {
            match condition_matches(
                property.condition.as_deref(),
                ConditionEnvironment {
                    properties: &self.properties,
                    unknown_properties: &self.unknown_properties,
                    unknown_import_taint: self.unknown_import_taint,
                    overrides: &self.overrides,
                    read_policy: &self.read_policy,
                },
                base,
                &mut self.metadata_files,
                &mut self.warnings,
                source_file,
            ) {
                TruthValue::False => continue,
                TruthValue::Unknown => {
                    self.incomplete = true;
                    self.mark_property_unknown(&property.name, source_file, source_provenance);
                    continue;
                }
                TruthValue::True => {}
            }
            if self
                .global_properties
                .contains(&property.name.to_ascii_lowercase())
            {
                continue;
            }
            let value = expand_value(
                &property.value,
                &property.name,
                &self.properties,
                &self.configured_ranges,
                &self.configured_properties,
                &self.property_provenance_ranges,
                &self.property_default_provenances,
                &self.unknown_properties,
                &mut self.warnings,
                source_file,
                source_provenance,
            );
            let trimmed = value.value.trim();
            let trim_start = value.value.len() - value.value.trim_start().len();
            let configured_ranges =
                trim_ranges(&value.configured_ranges, trim_start, trimmed.len());
            let provenance_ranges =
                trim_provenance_ranges(&value.provenance_ranges, trim_start, trimmed.len());
            self.set_property(
                &property.name,
                trimmed.to_string(),
                value.unknown,
                configured_ranges,
                value.explicit_dependency,
                provenance_ranges,
                source_provenance,
                source_file,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn set_property(
        &mut self,
        name: &str,
        value: String,
        unknown: bool,
        configured_ranges: Vec<Range<usize>>,
        explicit_dependency: bool,
        provenance_ranges: Vec<ProvenanceRange>,
        source_provenance: &ProjectPathProvenance,
        source_file: &Path,
    ) {
        let key = name.to_ascii_lowercase();
        if self.global_properties.contains(&key) {
            return;
        }
        let previous_bytes = self.properties.get(&key).map_or(0, String::len);
        let new_bytes = self
            .property_bytes
            .saturating_sub(previous_bytes)
            .saturating_add(value.len());
        if new_bytes > MAX_TOTAL_PROPERTY_BYTES {
            self.incomplete = true;
            self.warnings.push(format!(
                "property memory budget exceeded while evaluating {name} in {}; value ignored",
                source_file.display()
            ));
            return;
        }
        self.property_bytes = new_bytes;
        if unknown || value.contains(UNRESOLVED_MARKER) {
            self.unknown_properties.insert(key.clone());
        } else {
            self.unknown_properties.remove(&key);
        }
        if explicit_dependency {
            self.configured_properties.insert(key.clone());
        } else {
            self.configured_properties.remove(&key);
        }
        if configured_ranges.is_empty() {
            self.configured_ranges.remove(&key);
        } else {
            self.configured_ranges
                .insert(key.clone(), configured_ranges);
        }
        if provenance_ranges.is_empty() {
            self.property_provenance_ranges.remove(&key);
        } else {
            self.property_provenance_ranges
                .insert(key.clone(), provenance_ranges);
        }
        self.property_default_provenances
            .insert(key.clone(), source_provenance.clone());
        self.properties.insert(key, value);
    }

    fn taint_unknown_import(&mut self) {
        self.unknown_import_taint = true;
        let keys: Vec<String> = self.properties.keys().cloned().collect();
        for key in keys {
            if self.global_properties.contains(&key) {
                continue;
            }
            // MainSource identifies the project itself and is consumed after
            // the complete metadata stream has been evaluated. Keep an
            // already-established value available for that identity lookup,
            // but retain its evaluation taint so later conditions cannot use
            // it as definite evidence. A later definite assignment clears the
            // taint normally.
            if key.eq_ignore_ascii_case("mainsource") {
                self.unknown_properties.insert(key);
                continue;
            }
            let previous_bytes = self.properties.get(&key).map_or(0, String::len);
            self.property_bytes = self
                .property_bytes
                .saturating_sub(previous_bytes)
                .saturating_add(UNRESOLVED_MARKER.len_utf8());
            self.properties
                .insert(key.clone(), UNRESOLVED_MARKER.to_string());
            self.configured_ranges.remove(&key);
            self.property_provenance_ranges.remove(&key);
            self.unknown_properties.insert(key);
        }
    }

    fn unknown_installation_import(
        &mut self,
        source_file: &Path,
        reason: &str,
    ) -> Result<(), String> {
        self.incomplete = true;
        self.taint_unknown_import();
        if self.installation_data_mode {
            Err(format!(
                "unknown installation-data import in {}: {reason}",
                source_file.display()
            ))
        } else {
            Ok(())
        }
    }

    fn mark_property_unknown(
        &mut self,
        name: &str,
        source_file: &Path,
        source_provenance: &ProjectPathProvenance,
    ) {
        if self.global_properties.contains(&name.to_ascii_lowercase()) {
            return;
        }
        self.set_property(
            name,
            UNRESOLVED_MARKER.to_string(),
            true,
            Vec::new(),
            false,
            Vec::new(),
            source_provenance,
            source_file,
        );
        self.unknown_properties.insert(name.to_ascii_lowercase());
    }

    fn process_import(
        &mut self,
        import: Import,
        source_file: &Path,
        base: &Path,
        tracker: &mut ProjectReadTracker<'_>,
        source_provenance: &ProjectPathProvenance,
    ) -> Result<(), String> {
        let condition = condition_matches(
            import.condition.as_deref(),
            ConditionEnvironment {
                properties: &self.properties,
                unknown_properties: &self.unknown_properties,
                unknown_import_taint: self.unknown_import_taint,
                overrides: &self.overrides,
                read_policy: &self.read_policy,
            },
            base,
            &mut self.metadata_files,
            &mut self.warnings,
            source_file,
        );
        match condition {
            TruthValue::True => {}
            TruthValue::False => return Ok(()),
            TruthValue::Unknown => {
                self.incomplete = true;
                self.taint_unknown_import();
                return if self.installation_data_mode {
                    Err(format!(
                        "unknown installation-data import condition in {}: {}",
                        source_file.display(),
                        import.project
                    ))
                } else {
                    Ok(())
                };
            }
        }
        if !import_may_be_evaluated(&import.project) {
            let message = format!(
                "unsupported MSBuild import in {} (targets are not executed): {}",
                source_file.display(),
                import.project
            );
            if self.installation_data_mode {
                return Err(message);
            }
            self.warnings.push(format!(
                "ignored unsupported MSBuild import in {} (targets are not executed): {}",
                source_file.display(),
                import.project
            ));
            return Ok(());
        }
        let expanded = expand_value(
            &import.project,
            "",
            &self.properties,
            &self.configured_ranges,
            &self.configured_properties,
            &self.property_provenance_ranges,
            &self.property_default_provenances,
            &self.unknown_properties,
            &mut self.warnings,
            source_file,
            source_provenance,
        );
        if expanded.unknown || expanded.value.contains(UNRESOLVED_MARKER) {
            return self.unknown_installation_import(source_file, "unresolved import path");
        }
        let Some(resolved) = project_path_candidate(
            &expanded.value,
            base,
            &self.overrides,
            &mut self.warnings,
            "project import",
        ) else {
            return self.unknown_installation_import(source_file, "unresolvable import path");
        };
        let candidate = lexical_normalize(&resolved.path);
        if !is_supported_project_import(&candidate) {
            let message = format!(
                "unsupported MSBuild import in {} (targets are not executed): {}",
                source_file.display(),
                import.project
            );
            if self.installation_data_mode {
                return Err(message);
            }
            self.warnings.push(format!(
                "ignored unsupported MSBuild import in {} (targets are not executed): {}",
                source_file.display(),
                import.project
            ));
            return Ok(());
        }
        let path_status = resolve_existing_path_status_with_provenance(
            &candidate,
            &resolved,
            &expanded.value,
            &mut self.warnings,
            "project import",
        );
        let path = match &path_status {
            ExistingPathStatus::Found(path) => path,
            ExistingPathStatus::Missing => &candidate,
            ExistingPathStatus::Unresolvable => {
                return self
                    .unknown_installation_import(source_file, "unresolvable import location");
            }
        };
        let entry = inherit_path_provenance(
            ProjectPathEntry::resolved(path.clone(), &resolved, expanded.explicit_dependency),
            &expanded_path_provenance(&expanded, source_provenance),
        );
        if matches!(path_status, ExistingPathStatus::Found(_))
            && !self.read_policy.allows_entry(&entry)
        {
            self.warnings.push(format!(
                "ignored project import outside authorized read roots: {}",
                path.display()
            ));
            return self.unknown_installation_import(
                source_file,
                "import is outside authorized read roots",
            );
        }
        if !add_metadata_file(
            &mut self.metadata_files,
            path.clone(),
            &mut self.warnings,
            source_file,
            "project import",
        ) {
            return self.unknown_installation_import(source_file, "metadata import limit reached");
        }
        if self.import_count >= MAX_IMPORT_COUNT {
            self.warnings.push(format!(
                "project import limit ({MAX_IMPORT_COUNT}) reached while reading {}",
                source_file.display()
            ));
            return self.unknown_installation_import(source_file, "project import limit reached");
        }
        let ExistingPathStatus::Found(path) = path_status else {
            self.warnings.push(missing_path_warning(
                "project import",
                &expanded.value,
                &candidate,
                &resolved,
            ));
            return self.unknown_installation_import(source_file, "import file is missing");
        };
        if !self.active_imports.insert(path.clone()) {
            self.warnings.push(format!(
                "project import cycle ignored at {}",
                path.display()
            ));
            return self.unknown_installation_import(source_file, "project import cycle");
        }
        self.import_count += 1;
        let result =
            read_payload_with_tracker(&self.read_policy, &entry, MAX_IMPORT_BYTES, tracker)
                .and_then(|(contents, observation)| {
                    self.record_payload_observation(observation);
                    parse_xml_operations(&contents, &path)
                });
        let result = match result {
            Ok(operations) => {
                self.process_operations(operations, &path, tracker, &entry.provenance)
            }
            Err(error) => {
                let transient = tracker
                    .work_budget
                    .is_some_and(|budget| budget.is_transient_error(&error));
                self.incomplete = true;
                self.taint_unknown_import();
                self.warnings.push(format!(
                    "could not read project import {}: {error}",
                    path.display()
                ));
                if transient || self.installation_data_mode {
                    Err(error)
                } else {
                    Ok(())
                }
            }
        };
        self.active_imports.remove(&path);
        result
    }
}

#[derive(Debug)]
pub(super) struct DccReference {
    pub(super) include: String,
    pub(super) condition: Option<String>,
    pub(super) source_file: PathBuf,
    pub(super) source_provenance: ProjectPathProvenance,
}

#[derive(Debug)]
pub(super) struct Import {
    pub(super) project: String,
    pub(super) condition: Option<String>,
}

#[derive(Debug)]
pub(super) struct PropertyGroup {
    pub(super) condition: Option<String>,
    pub(super) properties: Vec<PropertyEntry>,
}

#[derive(Debug)]
pub(super) struct PropertyEntry {
    pub(super) name: String,
    pub(super) condition: Option<String>,
    pub(super) value: String,
}

#[derive(Debug)]
pub(super) enum XmlOperation {
    PropertyGroup(PropertyGroup),
    DccReference(DccReference),
    Import(Import),
    Unsupported(String),
}

fn is_supported_project_import(path: &Path) -> bool {
    extension_is(path, "optset") || extension_is(path, "props")
}

fn looks_like_supported_project_import(project: &str) -> bool {
    project
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .is_some_and(|name| {
            let name = name.to_ascii_lowercase();
            name.ends_with(".optset") || name.ends_with(".props")
        })
}

fn import_may_be_evaluated(project: &str) -> bool {
    let project = project.trim();
    if looks_like_supported_project_import(project) {
        return true;
    }
    let normalized = project.replace('\\', "/");
    let Some(name) = normalized.rsplit('/').next() else {
        return false;
    };
    if !name.contains("$(") {
        return false;
    }
    match name.rfind(')') {
        Some(end) => {
            let suffix = &name[end + 1..];
            suffix.is_empty()
                || matches!(suffix.to_ascii_lowercase().as_str(), ".props" | ".optset")
        }
        None => true,
    }
}

#[derive(Debug)]
enum FrameKind {
    Other {
        condition: Option<String>,
        in_target: bool,
    },
    PropertyGroup {
        operation: usize,
        condition: Option<String>,
    },
    Property {
        operation: usize,
        condition: Option<String>,
        in_target: bool,
        entry: PropertyEntry,
    },
}

impl FrameKind {
    fn condition(&self) -> Option<&str> {
        match self {
            Self::Other { condition, .. }
            | Self::PropertyGroup { condition, .. }
            | Self::Property { condition, .. } => condition.as_deref(),
        }
    }

    fn in_target(&self) -> bool {
        match self {
            Self::Other { in_target, .. } | Self::Property { in_target, .. } => *in_target,
            Self::PropertyGroup { .. } => false,
        }
    }
}

#[derive(Debug)]
struct XmlFrame {
    kind: FrameKind,
    text: String,
}

pub(super) fn parse_xml_operations(
    contents: &str,
    path: &Path,
) -> Result<Vec<XmlOperation>, String> {
    // Text is trimmed once per element: trimming each event would drop the
    // spaces around entity references, which arrive as separate events.
    let mut reader = Reader::from_str(contents);
    let mut buffer = Vec::new();
    let mut operations = Vec::new();
    let mut frames: Vec<XmlFrame> = Vec::new();

    loop {
        let event = reader
            .read_event_into(&mut buffer)
            .map_err(|error| format!("invalid XML in {}: {error}", path.display()))?;
        match event {
            Event::Start(start) => {
                let name = local_name(start.name().as_ref());
                let attributes = xml_attributes(&start)?;
                let parent_condition = frames
                    .last()
                    .and_then(|frame| frame.kind.condition().map(ToOwned::to_owned));
                let parent_in_target = frames.last().is_some_and(|frame| frame.kind.in_target());
                let own_condition = attributes.get("condition").cloned();
                let effective_condition =
                    combine_conditions(parent_condition.as_deref(), own_condition.as_deref());
                let in_target = parent_in_target || name.eq_ignore_ascii_case("target");
                let parent_group = frames.last().and_then(|frame| match &frame.kind {
                    FrameKind::PropertyGroup { operation, .. } => Some(*operation),
                    _ => None,
                });
                if name.eq_ignore_ascii_case("target") && !parent_in_target {
                    operations.push(XmlOperation::Unsupported(format!(
                        "ignored Target body in {}; build targets are not executed",
                        path.display()
                    )));
                }
                if in_target {
                    frames.push(XmlFrame {
                        kind: FrameKind::Other {
                            condition: effective_condition,
                            in_target: true,
                        },
                        text: String::new(),
                    });
                } else if name.eq_ignore_ascii_case("propertygroup") {
                    let operation = operations.len();
                    operations.push(XmlOperation::PropertyGroup(PropertyGroup {
                        condition: effective_condition.clone(),
                        properties: Vec::new(),
                    }));
                    frames.push(XmlFrame {
                        kind: FrameKind::PropertyGroup {
                            operation,
                            condition: effective_condition,
                        },
                        text: String::new(),
                    });
                } else if name.eq_ignore_ascii_case("import") {
                    operations.push(XmlOperation::Import(Import {
                        project: attributes.get("project").cloned().unwrap_or_default(),
                        condition: effective_condition,
                    }));
                    frames.push(XmlFrame {
                        kind: FrameKind::Other {
                            condition: None,
                            in_target: false,
                        },
                        text: String::new(),
                    });
                } else if name.eq_ignore_ascii_case("dccreference") {
                    operations.push(XmlOperation::DccReference(DccReference {
                        include: attributes.get("include").cloned().unwrap_or_default(),
                        condition: effective_condition,
                        source_file: path.to_path_buf(),
                        source_provenance: ProjectPathProvenance::LegacyNative,
                    }));
                    frames.push(XmlFrame {
                        kind: FrameKind::Other {
                            condition: None,
                            in_target: false,
                        },
                        text: String::new(),
                    });
                } else if (name.eq_ignore_ascii_case("option")
                    || name.eq_ignore_ascii_case("property"))
                    && attributes.contains_key("name")
                {
                    let operation = operations.len();
                    operations.push(XmlOperation::PropertyGroup(PropertyGroup {
                        condition: effective_condition.clone(),
                        properties: Vec::new(),
                    }));
                    frames.push(XmlFrame {
                        kind: FrameKind::Property {
                            operation,
                            condition: effective_condition,
                            in_target: false,
                            entry: PropertyEntry {
                                name: attributes.get("name").cloned().unwrap_or_default(),
                                condition: None,
                                value: attributes.get("value").cloned().unwrap_or_default(),
                            },
                        },
                        text: String::new(),
                    });
                } else if let Some(operation) = parent_group {
                    frames.push(XmlFrame {
                        kind: FrameKind::Property {
                            operation,
                            condition: frames
                                .last()
                                .and_then(|frame| frame.kind.condition().map(ToOwned::to_owned)),
                            in_target: false,
                            entry: PropertyEntry {
                                name,
                                condition: own_condition,
                                value: String::new(),
                            },
                        },
                        text: String::new(),
                    });
                } else {
                    frames.push(XmlFrame {
                        kind: FrameKind::Other {
                            condition: effective_condition,
                            in_target: false,
                        },
                        text: String::new(),
                    });
                }
            }
            Event::Empty(empty) => {
                let name = local_name(empty.name().as_ref());
                let attributes = xml_attributes(&empty)?;
                let parent_condition = frames
                    .last()
                    .and_then(|frame| frame.kind.condition().map(ToOwned::to_owned));
                let parent_in_target = frames.last().is_some_and(|frame| frame.kind.in_target());
                let own_condition = attributes.get("condition").cloned();
                let effective_condition =
                    combine_conditions(parent_condition.as_deref(), own_condition.as_deref());
                if parent_in_target || name.eq_ignore_ascii_case("target") {
                    if name.eq_ignore_ascii_case("target") && !parent_in_target {
                        operations.push(XmlOperation::Unsupported(format!(
                            "ignored Target body in {}; build targets are not executed",
                            path.display()
                        )));
                    }
                    buffer.clear();
                    continue;
                }
                let parent_group = frames.last().and_then(|frame| match &frame.kind {
                    FrameKind::PropertyGroup { operation, .. } => Some(*operation),
                    _ => None,
                });
                if name.eq_ignore_ascii_case("import") {
                    operations.push(XmlOperation::Import(Import {
                        project: attributes.get("project").cloned().unwrap_or_default(),
                        condition: effective_condition,
                    }));
                } else if name.eq_ignore_ascii_case("dccreference") {
                    operations.push(XmlOperation::DccReference(DccReference {
                        include: attributes.get("include").cloned().unwrap_or_default(),
                        condition: effective_condition,
                        source_file: path.to_path_buf(),
                        source_provenance: ProjectPathProvenance::LegacyNative,
                    }));
                } else if (name.eq_ignore_ascii_case("option")
                    || name.eq_ignore_ascii_case("property"))
                    && attributes.contains_key("name")
                {
                    operations.push(XmlOperation::PropertyGroup(PropertyGroup {
                        condition: effective_condition,
                        properties: vec![PropertyEntry {
                            name: attributes.get("name").cloned().unwrap_or_default(),
                            condition: None,
                            value: attributes.get("value").cloned().unwrap_or_default(),
                        }],
                    }));
                } else if let Some(operation) = parent_group {
                    push_property(
                        &mut operations,
                        operation,
                        PropertyEntry {
                            name,
                            condition: own_condition,
                            value: String::new(),
                        },
                    );
                }
            }
            Event::Text(text) => {
                if let Some(frame) = frames.last_mut()
                    && matches!(frame.kind, FrameKind::Property { .. })
                {
                    frame.text.push_str(&text);
                }
            }
            Event::GeneralRef(reference) => {
                if let Some(frame) = frames.last_mut()
                    && matches!(frame.kind, FrameKind::Property { .. })
                {
                    let resolved = xml_reference_text(&reference).map_err(|error| {
                        format!("invalid XML text in {}: {error}", path.display())
                    })?;
                    frame.text.push_str(&resolved);
                }
            }
            Event::CData(text) => {
                if let Some(frame) = frames.last_mut()
                    && matches!(frame.kind, FrameKind::Property { .. })
                {
                    frame.text.push_str(&text);
                }
            }
            Event::End(_) => {
                let Some(frame) = frames.pop() else {
                    return Err(format!("unexpected XML closing tag in {}", path.display()));
                };
                if let FrameKind::Property {
                    operation,
                    mut entry,
                    ..
                } = frame.kind
                {
                    entry.value = frame.text.trim().to_string();
                    push_property(&mut operations, operation, entry);
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    if !frames.is_empty() {
        return Err(format!("unterminated XML element in {}", path.display()));
    }
    Ok(operations)
}

fn push_property(operations: &mut [XmlOperation], operation: usize, entry: PropertyEntry) {
    if let Some(XmlOperation::PropertyGroup(group)) = operations.get_mut(operation) {
        group.properties.push(entry);
    }
}

pub(super) fn local_name(name: &str) -> String {
    name.rsplit(':').next().unwrap_or_default().to_string()
}

/// The text an XML entity or character reference stands for.
pub(super) fn xml_reference_text(reference: &BytesRef<'_>) -> Result<String, String> {
    if let Some(character) = reference
        .resolve_char_ref()
        .map_err(|error| error.to_string())?
    {
        return Ok(character.to_string());
    }
    quick_xml::escape::resolve_predefined_entity(reference)
        .map(str::to_owned)
        .ok_or_else(|| format!("unknown XML entity `&{};`", &**reference))
}

pub(super) fn xml_attributes(start: &BytesStart<'_>) -> Result<HashMap<String, String>, String> {
    let mut attributes = HashMap::new();
    for attribute in start.attributes().with_checks(false) {
        let attribute = attribute.map_err(|error| format!("invalid XML attribute: {error}"))?;
        let name = local_name(attribute.key.as_ref()).to_ascii_lowercase();
        let value = attribute
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|error| format!("invalid XML attribute value: {error}"))?;
        attributes.insert(name, value.into_owned());
    }
    Ok(attributes)
}

#[derive(Debug)]
pub(super) struct ExpandedValue {
    pub(super) value: String,
    pub(super) unknown: bool,
    pub(super) explicit_dependency: bool,
    pub(super) configured_ranges: Vec<Range<usize>>,
    pub(super) provenance_ranges: Vec<ProvenanceRange>,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn expand_value(
    value: &str,
    current_property: &str,
    properties: &HashMap<String, String>,
    property_ranges: &HashMap<String, Vec<Range<usize>>>,
    configured_properties: &HashSet<String>,
    property_provenance_ranges: &HashMap<String, Vec<ProvenanceRange>>,
    property_default_provenances: &HashMap<String, ProjectPathProvenance>,
    unknown_properties: &HashSet<String>,
    warnings: &mut Vec<String>,
    source_file: &Path,
    source_provenance: &ProjectPathProvenance,
) -> ExpandedValue {
    let mut expanded = String::new();
    let mut unknown = false;
    let mut explicit_dependency = false;
    let mut configured_ranges = Vec::new();
    let mut provenance_ranges = Vec::new();
    let mut cursor = 0;
    while let Some(relative_start) = value[cursor..].find("$(") {
        let start = cursor + relative_start;
        if !append_expansion_with_provenance(
            &mut expanded,
            &value[cursor..start],
            warnings,
            source_file,
            &mut provenance_ranges,
            source_provenance,
        ) {
            return ExpandedValue {
                value: UNRESOLVED_MARKER.to_string(),
                unknown: true,
                explicit_dependency,
                configured_ranges: Vec::new(),
                provenance_ranges: Vec::new(),
            };
        }
        let Some(relative_end) = value[start + 2..].find(')') else {
            warnings.push(format!(
                "unsupported unclosed property expansion in {}",
                source_file.display()
            ));
            return ExpandedValue {
                value: UNRESOLVED_MARKER.to_string(),
                unknown: true,
                explicit_dependency,
                configured_ranges: Vec::new(),
                provenance_ranges: Vec::new(),
            };
        };
        let end = start + 2 + relative_end;
        let name = value[start + 2..end].trim();
        let key = name.to_ascii_lowercase();
        if name.eq_ignore_ascii_case("thisfiledirectory")
            || name.eq_ignore_ascii_case("msbuildthisfiledirectory")
        {
            let directory = source_file
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_string_lossy()
                .replace('\\', "/");
            let directory = format!("{}/", directory.trim_end_matches('/'));
            if !append_expansion_with_provenance(
                &mut expanded,
                &directory,
                warnings,
                source_file,
                &mut provenance_ranges,
                source_provenance,
            ) {
                return ExpandedValue {
                    value: UNRESOLVED_MARKER.to_string(),
                    unknown: true,
                    explicit_dependency,
                    configured_ranges: Vec::new(),
                    provenance_ranges: Vec::new(),
                };
            }
        } else if let Some(replacement) = properties.get(&key) {
            let replacement_start = expanded.len();
            if !append_expansion(&mut expanded, replacement, warnings, source_file) {
                return ExpandedValue {
                    value: UNRESOLVED_MARKER.to_string(),
                    unknown: true,
                    explicit_dependency,
                    configured_ranges: Vec::new(),
                    provenance_ranges: Vec::new(),
                };
            }
            let replacement_end = expanded.len();
            if let Some(ranges) = property_provenance_ranges.get(&key) {
                for range in ranges {
                    if range.range.start <= range.range.end && range.range.end <= replacement.len()
                    {
                        provenance_ranges.push(ProvenanceRange {
                            range: (replacement_start + range.range.start)
                                ..(replacement_start + range.range.end),
                            provenance: range.provenance.clone(),
                        });
                    }
                }
            } else if replacement_start < replacement_end {
                provenance_ranges.push(ProvenanceRange {
                    range: replacement_start..replacement_end,
                    provenance: property_default_provenances
                        .get(&key)
                        .cloned()
                        .unwrap_or_else(|| source_provenance.clone()),
                });
            } else if let Some(provenance) = property_default_provenances.get(&key) {
                provenance_ranges.push(ProvenanceRange {
                    range: replacement_start..replacement_end,
                    provenance: provenance.clone(),
                });
            }
            if let Some(ranges) = property_ranges.get(&key) {
                for range in ranges {
                    if range.start < range.end && range.end <= replacement.len() {
                        configured_ranges.push(
                            (replacement_start + range.start)..(replacement_start + range.end),
                        );
                    }
                }
            }
            explicit_dependency |= configured_properties.contains(&key)
                || property_ranges
                    .get(&key)
                    .is_some_and(|ranges| !ranges.is_empty());
            unknown |= replacement.contains(UNRESOLVED_MARKER) || unknown_properties.contains(&key);
        } else if name.eq_ignore_ascii_case(current_property) {
            // Delphi project files commonly terminate list properties with
            // their own unset value. This is an intentional empty default.
            unknown |= unknown_properties.contains(&key);
        } else {
            warnings.push(format!(
                "unresolved property path $({name}) in {}",
                source_file.display()
            ));
            unknown = true;
            if !append_expansion(
                &mut expanded,
                &UNRESOLVED_MARKER.to_string(),
                warnings,
                source_file,
            ) {
                return ExpandedValue {
                    value: UNRESOLVED_MARKER.to_string(),
                    unknown: true,
                    explicit_dependency,
                    configured_ranges: Vec::new(),
                    provenance_ranges: Vec::new(),
                };
            }
        }
        cursor = end + 1;
    }
    if !append_expansion_with_provenance(
        &mut expanded,
        &value[cursor..],
        warnings,
        source_file,
        &mut provenance_ranges,
        source_provenance,
    ) {
        return ExpandedValue {
            value: UNRESOLVED_MARKER.to_string(),
            unknown: true,
            explicit_dependency,
            configured_ranges: Vec::new(),
            provenance_ranges: Vec::new(),
        };
    }
    ExpandedValue {
        value: expanded,
        unknown,
        explicit_dependency,
        configured_ranges,
        provenance_ranges,
    }
}

fn append_expansion(
    target: &mut String,
    addition: &str,
    warnings: &mut Vec<String>,
    source_file: &Path,
) -> bool {
    let exceeds_budget = match target.len().checked_add(addition.len()) {
        Some(length) => length > MAX_EXPANDED_VALUE_BYTES,
        None => true,
    };
    if exceeds_budget {
        warnings.push(format!(
            "property expansion exceeds the {} byte per-value budget in {}; value omitted",
            MAX_EXPANDED_VALUE_BYTES,
            source_file.display()
        ));
        return false;
    }
    target.push_str(addition);
    true
}

fn append_expansion_with_provenance(
    target: &mut String,
    addition: &str,
    warnings: &mut Vec<String>,
    source_file: &Path,
    provenance_ranges: &mut Vec<ProvenanceRange>,
    provenance: &ProjectPathProvenance,
) -> bool {
    let start = target.len();
    if !append_expansion(target, addition, warnings, source_file) {
        return false;
    }
    if start < target.len() {
        provenance_ranges.push(ProvenanceRange {
            range: start..target.len(),
            provenance: provenance.clone(),
        });
    }
    true
}

#[derive(Debug, Clone)]
struct ConditionValue {
    value: String,
    unknown: bool,
}

pub(super) struct ConditionEnvironment<'a> {
    pub(super) properties: &'a HashMap<String, String>,
    pub(super) unknown_properties: &'a HashSet<String>,
    pub(super) unknown_import_taint: bool,
    pub(super) overrides: &'a EffectiveOverrides,
    pub(super) read_policy: &'a ReadPolicy,
}

fn expand_condition_value(
    value: &str,
    properties: &HashMap<String, String>,
    unknown_properties: &HashSet<String>,
    unknown_import_taint: bool,
    source_file: &Path,
    warnings: &mut Vec<String>,
) -> ConditionValue {
    let mut expanded = String::new();
    let mut cursor = 0;
    let mut missing = false;
    while let Some(relative_start) = value[cursor..].find("$(") {
        let start = cursor + relative_start;
        if !append_expansion(&mut expanded, &value[cursor..start], warnings, source_file) {
            return ConditionValue {
                value: String::new(),
                unknown: true,
            };
        }
        let Some(relative_end) = value[start + 2..].find(')') else {
            return ConditionValue {
                value: String::new(),
                unknown: true,
            };
        };
        let end = start + 2 + relative_end;
        let name = value[start + 2..end].trim();
        if name.eq_ignore_ascii_case("thisfiledirectory")
            || name.eq_ignore_ascii_case("msbuildthisfiledirectory")
        {
            let directory = source_file
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_string_lossy()
                .replace('\\', "/");
            let directory = format!("{}/", directory.trim_end_matches('/'));
            if !append_expansion(&mut expanded, &directory, warnings, source_file) {
                return ConditionValue {
                    value: String::new(),
                    unknown: true,
                };
            }
        } else if let Some(replacement) = properties.get(&name.to_ascii_lowercase()) {
            if !append_expansion(&mut expanded, replacement, warnings, source_file) {
                return ConditionValue {
                    value: String::new(),
                    unknown: true,
                };
            }
            missing |= replacement.contains(UNRESOLVED_MARKER)
                || unknown_properties.contains(&name.to_ascii_lowercase());
        } else if unknown_import_taint || !is_known_project_local_configuration_property(name) {
            missing = true;
            if !append_expansion(
                &mut expanded,
                &UNRESOLVED_MARKER.to_string(),
                warnings,
                source_file,
            ) {
                return ConditionValue {
                    value: String::new(),
                    unknown: true,
                };
            }
        }
        cursor = end + 1;
    }
    if !append_expansion(&mut expanded, &value[cursor..], warnings, source_file) {
        return ConditionValue {
            value: String::new(),
            unknown: true,
        };
    }
    ConditionValue {
        value: expanded,
        unknown: missing,
    }
}

fn combine_conditions(parent: Option<&str>, own: Option<&str>) -> Option<String> {
    match (
        parent.map(str::trim).filter(|value| !value.is_empty()),
        own.map(str::trim).filter(|value| !value.is_empty()),
    ) {
        (None, None) => None,
        (Some(parent), None) => Some(parent.to_string()),
        (None, Some(own)) => Some(own.to_string()),
        (Some(parent), Some(own)) => Some(format!("({parent}) And ({own})")),
    }
}

pub(super) fn condition_matches(
    condition: Option<&str>,
    environment: ConditionEnvironment<'_>,
    base: &Path,
    metadata_files: &mut Vec<PathBuf>,
    warnings: &mut Vec<String>,
    source_file: &Path,
) -> TruthValue {
    let Some(condition) = condition.map(str::trim).filter(|value| !value.is_empty()) else {
        return TruthValue::True;
    };
    let tokens = match tokenize_condition(condition) {
        Ok(tokens) => tokens,
        Err(error) => {
            warnings.push(format!(
                "unsupported project condition in {}: {error}: {condition}",
                source_file.display()
            ));
            return TruthValue::Unknown;
        }
    };
    let mut parser = ConditionParser {
        tokens,
        position: 0,
        properties: environment.properties,
        unknown_properties: environment.unknown_properties,
        unknown_import_taint: environment.unknown_import_taint,
        overrides: environment.overrides,
        read_policy: environment.read_policy,
        base,
        metadata_files,
        warnings,
        source_file,
        unknown_seen: false,
    };
    let result = parser.parse();
    let unknown = parser.unknown_seen;
    match result {
        Ok(TruthValue::True) => TruthValue::True,
        Ok(TruthValue::False) => {
            if unknown {
                parser.warnings.push(format!(
                    "unknown project condition in {}: {condition}",
                    parser.source_file.display()
                ));
            }
            TruthValue::False
        }
        Ok(TruthValue::Unknown) => {
            parser.warnings.push(format!(
                "unknown project condition in {}: {condition}",
                parser.source_file.display()
            ));
            TruthValue::Unknown
        }
        Err(error) => {
            parser.warnings.push(format!(
                "unsupported project condition in {}: {error}: {condition}",
                parser.source_file.display()
            ));
            TruthValue::Unknown
        }
    }
}

fn is_known_project_local_configuration_property(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == "config"
        || name == "platform"
        || name == "base"
        || name.starts_with("base_")
        || name.starts_with("cfg_")
        || name == "cfgparent"
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TruthValue {
    True,
    False,
    Unknown,
}

impl TruthValue {
    pub(super) fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::False, _) | (_, Self::False) => Self::False,
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            (Self::True, Self::True) => Self::True,
        }
    }

    pub(super) fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::True, _) | (_, Self::True) => Self::True,
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            (Self::False, Self::False) => Self::False,
        }
    }
}

#[derive(Debug, Clone)]
enum ConditionToken {
    Word(String),
    Quoted(String),
    Equals,
    NotEquals,
    LeftParen,
    RightParen,
    Comma,
}

fn tokenize_condition(condition: &str) -> Result<Vec<ConditionToken>, String> {
    let mut tokens = Vec::new();
    let mut cursor = 0;
    while cursor < condition.len() {
        let rest = &condition[cursor..];
        let Some(first) = rest.chars().next() else {
            break;
        };
        if first.is_whitespace() {
            cursor += first.len_utf8();
            continue;
        }
        if rest.starts_with("==") {
            tokens.push(ConditionToken::Equals);
            cursor += 2;
            continue;
        }
        if rest.starts_with("!=") {
            tokens.push(ConditionToken::NotEquals);
            cursor += 2;
            continue;
        }
        match first {
            '(' => {
                tokens.push(ConditionToken::LeftParen);
                cursor += 1;
            }
            ')' => {
                tokens.push(ConditionToken::RightParen);
                cursor += 1;
            }
            ',' => {
                tokens.push(ConditionToken::Comma);
                cursor += 1;
            }
            '\'' | '"' => {
                let quote = first;
                cursor += quote.len_utf8();
                let start = cursor;
                let mut value = String::new();
                let mut closed = false;
                while cursor < condition.len() {
                    let current = condition[cursor..]
                        .chars()
                        .next()
                        .expect("cursor is inside condition");
                    cursor += current.len_utf8();
                    if current == quote {
                        closed = true;
                        break;
                    }
                    value.push(current);
                }
                if !closed {
                    return Err(format!("unterminated quoted value at byte {start}"));
                }
                tokens.push(ConditionToken::Quoted(value));
            }
            _ => {
                let start = cursor;
                while cursor < condition.len() {
                    let current = condition[cursor..]
                        .chars()
                        .next()
                        .expect("cursor is inside condition");
                    if current.is_whitespace() || matches!(current, '(' | ')' | ',' | '!' | '=') {
                        break;
                    }
                    cursor += current.len_utf8();
                }
                if cursor == start {
                    return Err(format!("unexpected character at byte {cursor}"));
                }
                let word = &condition[start..cursor];
                tokens.push(ConditionToken::Word(word.to_string()));
            }
        }
    }
    Ok(tokens)
}

struct ConditionParser<'a> {
    tokens: Vec<ConditionToken>,
    position: usize,
    properties: &'a HashMap<String, String>,
    unknown_properties: &'a HashSet<String>,
    unknown_import_taint: bool,
    overrides: &'a EffectiveOverrides,
    read_policy: &'a ReadPolicy,
    base: &'a Path,
    metadata_files: &'a mut Vec<PathBuf>,
    warnings: &'a mut Vec<String>,
    source_file: &'a Path,
    unknown_seen: bool,
}

impl ConditionParser<'_> {
    fn parse(&mut self) -> Result<TruthValue, String> {
        let value = self.parse_or()?;
        if self.position != self.tokens.len() {
            return Err("trailing tokens".to_string());
        }
        Ok(value)
    }

    fn parse_or(&mut self) -> Result<TruthValue, String> {
        let mut value = self.parse_and()?;
        while self.take_operator("or") {
            value = value.or(self.parse_and()?);
        }
        Ok(value)
    }

    fn parse_and(&mut self) -> Result<TruthValue, String> {
        let mut value = self.parse_primary()?;
        while self.take_operator("and") {
            value = value.and(self.parse_primary()?);
        }
        Ok(value)
    }

    fn parse_primary(&mut self) -> Result<TruthValue, String> {
        if self.take_token(|token| matches!(token, ConditionToken::LeftParen)) {
            let value = self.parse_or()?;
            if !self.take_token(|token| matches!(token, ConditionToken::RightParen)) {
                return Err("missing closing parenthesis".to_string());
            }
            return Ok(value);
        }
        if self.peek_word("exists") {
            self.position += 1;
            if !self.take_token(|token| matches!(token, ConditionToken::LeftParen)) {
                return Err("Exists requires parentheses".to_string());
            }
            let argument = self.parse_value()?;
            if !self.take_token(|token| matches!(token, ConditionToken::RightParen)) {
                return Err("Exists is missing its closing parenthesis".to_string());
            }
            if argument.unknown {
                self.unknown_seen = true;
                return Ok(TruthValue::Unknown);
            }
            let Some(resolved) = project_path_candidate(
                &argument.value,
                self.base,
                self.overrides,
                self.warnings,
                "Exists condition",
            ) else {
                self.unknown_seen = true;
                return Ok(TruthValue::Unknown);
            };
            let candidate = lexical_normalize(&resolved.path);
            let entry = ProjectPathEntry::resolved(candidate.clone(), &resolved, true);
            if !self.read_policy.allows_location(&entry) {
                self.unknown_seen = true;
                return Ok(TruthValue::Unknown);
            }
            let path_status = resolve_existing_path_status_with_provenance(
                &candidate,
                &resolved,
                &argument.value,
                self.warnings,
                "Exists condition",
            );
            let path = match &path_status {
                ExistingPathStatus::Found(path) => path,
                ExistingPathStatus::Missing | ExistingPathStatus::Unresolvable => &candidate,
            };
            if !add_metadata_file(
                self.metadata_files,
                path.clone(),
                self.warnings,
                self.source_file,
                "Exists condition",
            ) {
                self.unknown_seen = true;
                return Ok(TruthValue::Unknown);
            }
            return Ok(match path_status {
                ExistingPathStatus::Found(_) => TruthValue::True,
                ExistingPathStatus::Missing => TruthValue::False,
                ExistingPathStatus::Unresolvable => TruthValue::Unknown,
            });
        }

        let left = self.parse_value()?;
        let operator = if self.take_token(|token| matches!(token, ConditionToken::Equals)) {
            true
        } else if self.take_token(|token| matches!(token, ConditionToken::NotEquals)) {
            false
        } else {
            return Err("condition requires == or !=".to_string());
        };
        let right = self.parse_value()?;
        if left.unknown || right.unknown {
            self.unknown_seen = true;
            return Ok(TruthValue::Unknown);
        }
        let equal = left.value.eq_ignore_ascii_case(&right.value);
        Ok(if operator {
            if equal {
                TruthValue::True
            } else {
                TruthValue::False
            }
        } else if equal {
            TruthValue::False
        } else {
            TruthValue::True
        })
    }

    fn parse_value(&mut self) -> Result<ConditionValue, String> {
        let token = self
            .tokens
            .get(self.position)
            .ok_or_else(|| "missing condition value".to_string())?
            .clone();
        self.position += 1;
        match token {
            ConditionToken::Word(value) | ConditionToken::Quoted(value) => {
                let expanded = expand_condition_value(
                    &value,
                    self.properties,
                    self.unknown_properties,
                    self.unknown_import_taint,
                    self.source_file,
                    self.warnings,
                );
                self.unknown_seen |= expanded.unknown;
                Ok(expanded)
            }
            _ => Err("expected a condition value".to_string()),
        }
    }

    fn peek_word(&self, expected: &str) -> bool {
        matches!(
            self.tokens.get(self.position),
            Some(ConditionToken::Word(value)) if value.eq_ignore_ascii_case(expected)
        )
    }

    fn take_operator(&mut self, expected: &str) -> bool {
        if self.peek_word(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn take_token(&mut self, predicate: impl FnOnce(&ConditionToken) -> bool) -> bool {
        if self.tokens.get(self.position).is_some_and(predicate) {
            self.position += 1;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn xml_property_text_resolves_entities_without_losing_spaces() {
        let xml = r#"<Project><PropertyGroup Condition="'$(Config)'=='R &amp; D'"><DCC_UnitSearchPath>C:\R &amp; D\lib;&#36;(BDS)</DCC_UnitSearchPath></PropertyGroup></Project>"#;
        let operations = super::parse_xml_operations(xml, std::path::Path::new("Entities.dproj"))
            .expect("well-formed XML");
        let Some(super::XmlOperation::PropertyGroup(group)) = operations.first() else {
            panic!("expected a property group, got {operations:?}");
        };
        assert_eq!(group.condition.as_deref(), Some("'$(Config)'=='R & D'"));
        assert_eq!(group.properties[0].value, r"C:\R & D\lib;$(BDS)");
    }

    #[test]
    fn xml_property_text_rejects_unknown_entities() {
        let xml = "<Project><PropertyGroup><Define>A&bogus;B</Define></PropertyGroup></Project>";
        assert!(super::parse_xml_operations(xml, std::path::Path::new("Unknown.dproj")).is_err());
    }
}

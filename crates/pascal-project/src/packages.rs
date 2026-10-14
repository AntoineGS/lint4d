use super::assembly::{
    add_unique_project_path_entry, add_unit_candidate, canonical_package_name, canonical_unit_name,
    complete_metadata_observations, expanded_path_provenance, inherit_path_provenance,
};
use super::authorization::{
    extension_is, is_compiled_reference, lexical_normalize, project_path_candidate,
    resolve_existing_path_status_with_provenance,
};
use super::membership::{PascalToken, lex_pascal, parse_unit_membership};
use super::msbuild_eval::{expand_value, parse_xml_operations};
use super::*;

/// Metadata extracted from one source package descriptor. The descriptor is
/// parsed only after a project import has failed the ordinary unit lookup;
/// `metadata_files` contains every project/option-set dependency used to
/// produce the result so callers can detect changes without file events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageMetadata {
    pub units: HashMap<String, Vec<PathBuf>>,
    pub unit_entries: HashMap<String, Vec<ProjectPathEntry>>,
    pub warnings: Vec<String>,
    /// Whether metadata evaluation omitted a path or otherwise could not prove
    /// the package's source catalogue complete.  Warnings are retained for
    /// diagnostics, but callers must use this flag for fail-closed lookup.
    pub incomplete: bool,
    pub metadata_files: Vec<PathBuf>,
    pub metadata_observations: Vec<MetadataObservation>,
}

impl PackageMetadata {
    pub fn visit_recovery_payload(
        &self,
        visit: &mut dyn FnMut(usize) -> Result<(), String>,
    ) -> Result<(), String> {
        for (name, paths) in &self.units {
            visit(name.len())?;
            for path in paths {
                visit(path.as_os_str().len())?;
            }
        }
        for (name, entries) in &self.unit_entries {
            visit(name.len())?;
            for entry in entries {
                entry.visit_recovery_payload(visit)?;
            }
        }
        for warning in &self.warnings {
            visit(warning.len())?;
        }
        for path in &self.metadata_files {
            visit(path.as_os_str().len())?;
        }
        for observation in &self.metadata_observations {
            observation.visit_recovery_payload(visit)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PackageMetadataRead {
    pub metadata: PackageMetadata,
    pub observations: Vec<ProjectReadObservation>,
}
#[allow(dead_code)]
pub fn read_package_metadata(
    path: &Path,
    options: &ProjectOptions,
    overrides: &EffectiveOverrides,
    read_policy: &ReadPolicy,
    entry: &ProjectPathEntry,
) -> Result<PackageMetadata, String> {
    read_package_metadata_with_observations(path, options, overrides, read_policy, entry)
        .map(|read| read.metadata)
}

pub fn read_package_metadata_with_observations(
    path: &Path,
    options: &ProjectOptions,
    overrides: &EffectiveOverrides,
    read_policy: &ReadPolicy,
    entry: &ProjectPathEntry,
) -> Result<PackageMetadataRead, String> {
    read_package_metadata_with_observations_and_work_budget(
        path,
        options,
        overrides,
        read_policy,
        entry,
        None,
    )
}

pub fn read_package_metadata_with_observations_and_work_budget(
    path: &Path,
    options: &ProjectOptions,
    overrides: &EffectiveOverrides,
    read_policy: &ReadPolicy,
    entry: &ProjectPathEntry,
    work_budget: Option<&dyn ProjectWorkBudget>,
) -> Result<PackageMetadataRead, String> {
    if !project_paths_equal(&entry.path, path) {
        return Err(format!(
            "package metadata entry does not match descriptor {}",
            path.display()
        ));
    }
    let mut tracker = ProjectReadTracker::with_budget(work_budget, &[]);
    let (contents, descriptor_observation) =
        read_payload_with_tracker(read_policy, entry, MAX_PACKAGE_METADATA_BYTES, &mut tracker)
            .map_err(|error| {
                format!(
                    "could not read package metadata {}: {error}",
                    path.display()
                )
            })?;
    let mut metadata = if extension_is(path, "dpk") {
        parse_dpk_metadata(path, &contents, options, overrides, read_policy, entry)
    } else if extension_is(path, "dproj") {
        parse_dproj_package_metadata(
            path,
            &contents,
            options,
            overrides,
            read_policy,
            entry,
            &mut tracker,
        )
    } else {
        Err(format!(
            "unsupported package descriptor extension: {}",
            path.display()
        ))
    }?;
    if let Some(work_budget) = work_budget {
        work_budget.check_cancelled()?;
    }
    metadata.incomplete |= metadata
        .warnings
        .iter()
        .any(|warning| package_metadata_warning_is_incomplete(warning));
    add_metadata_observation(&mut metadata.metadata_observations, descriptor_observation);
    metadata.metadata_observations =
        complete_metadata_observations(&metadata.metadata_files, metadata.metadata_observations);
    Ok(PackageMetadataRead {
        metadata,
        observations: tracker.observations,
    })
}

fn package_metadata_warning_is_incomplete(warning: &str) -> bool {
    warning.contains("ignored package path outside authorized read roots")
}

fn parse_dpk_metadata(
    path: &Path,
    contents: &str,
    _options: &ProjectOptions,
    overrides: &EffectiveOverrides,
    read_policy: &ReadPolicy,
    entry: &ProjectPathEntry,
) -> Result<PackageMetadata, String> {
    if declared_package_name(contents).is_none() {
        return Err(format!(
            "package descriptor {} has no package declaration",
            path.display()
        ));
    };
    // The bounded filename catalogue selected this descriptor by the package
    // name requested by the project. Its header is validated as package
    // syntax, but a legacy header spelling does not replace that identity.
    let membership = parse_unit_membership(contents);
    let mut metadata = PackageMetadata::default();
    metadata.metadata_files.push(path.to_path_buf());
    if !membership.exhaustive {
        metadata.incomplete = true;
        metadata.warnings.push(format!(
            "package {} contains membership is incomplete",
            path.display()
        ));
    }
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    for (unit_name, raw_path) in membership.explicit_paths {
        let Some(unit_entry) = package_unit_entry(
            &raw_path,
            base,
            overrides,
            &mut metadata.warnings,
            "package contains path",
            &entry.provenance,
            read_policy,
            &mut metadata.incomplete,
        ) else {
            continue;
        };
        add_package_unit_candidate(&mut metadata, canonical_unit_name(&unit_name), unit_entry);
    }
    Ok(metadata)
}

fn parse_dproj_package_metadata(
    path: &Path,
    contents: &str,
    options: &ProjectOptions,
    overrides: &EffectiveOverrides,
    read_policy: &ReadPolicy,
    entry: &ProjectPathEntry,
    tracker: &mut ProjectReadTracker,
) -> Result<PackageMetadata, String> {
    let project_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut builder = ProjectBuilder::new(
        options,
        overrides,
        Vec::new(),
        project_dir.to_path_buf(),
        read_policy.clone(),
    );
    let operations = parse_xml_operations(contents, path)?;
    builder.process_operations(operations, path, tracker, &entry.provenance)?;

    let Some(main_source) = builder.property("mainsource") else {
        return Err(format!(
            "package project {} has no package MainSource",
            path.display()
        ));
    };
    let main_source = main_source.replace('\\', "/");
    let main_path = Path::new(&main_source);
    if !extension_is(main_path, "dpk") {
        return Err(format!(
            "package project {} does not identify a DPK MainSource",
            path.display()
        ));
    }

    let mut metadata = PackageMetadata {
        warnings: builder.warnings,
        incomplete: builder.incomplete,
        metadata_files: vec![path.to_path_buf()],
        ..PackageMetadata::default()
    };
    if let Some(main_source) = package_unit_entry(
        &main_source,
        project_dir,
        overrides,
        &mut metadata.warnings,
        "package main source",
        &entry.provenance,
        read_policy,
        &mut metadata.incomplete,
    ) {
        metadata.metadata_files.push(main_source.path);
    }
    metadata.metadata_files.extend(builder.metadata_files);
    metadata
        .metadata_observations
        .extend(builder.metadata_observations);
    for reference in builder.references {
        if is_compiled_reference(&reference.include) {
            continue;
        }
        let expanded = expand_value(
            &reference.include,
            "",
            &builder.properties,
            &builder.configured_ranges,
            &builder.configured_properties,
            &builder.property_provenance_ranges,
            &builder.property_default_provenances,
            &builder.unknown_properties,
            &mut metadata.warnings,
            &reference.source_file,
            &reference.source_provenance,
        );
        if expanded.unknown || expanded.value.contains(UNRESOLVED_MARKER) {
            metadata.incomplete = true;
            continue;
        }
        if is_compiled_reference(&expanded.value) {
            continue;
        }
        let provenance = expanded_path_provenance(&expanded, &reference.source_provenance);
        let Some(unit_entry) = package_unit_entry(
            &expanded.value,
            project_dir,
            overrides,
            &mut metadata.warnings,
            "package project reference",
            &provenance,
            read_policy,
            &mut metadata.incomplete,
        ) else {
            continue;
        };
        let Some(stem) = unit_entry.path.file_stem() else {
            metadata.incomplete = true;
            continue;
        };
        add_package_unit_candidate(
            &mut metadata,
            canonical_unit_name(&stem.to_string_lossy()),
            unit_entry,
        );
    }
    Ok(metadata)
}

#[allow(clippy::too_many_arguments)]
fn package_unit_entry(
    raw: &str,
    base: &Path,
    overrides: &EffectiveOverrides,
    warnings: &mut Vec<String>,
    kind: &str,
    parent_provenance: &ProjectPathProvenance,
    read_policy: &ReadPolicy,
    incomplete: &mut bool,
) -> Option<ProjectPathEntry> {
    let raw = raw.trim();
    if raw.is_empty() || raw.contains(UNRESOLVED_MARKER) {
        *incomplete = true;
        return None;
    }
    let Some(resolved) = project_path_candidate(raw, base, overrides, warnings, kind) else {
        *incomplete = true;
        return None;
    };
    let candidate = lexical_normalize(&resolved.path);
    let path = match resolve_existing_path_status_with_provenance(
        &candidate, &resolved, raw, warnings, kind,
    ) {
        ExistingPathStatus::Found(path) => path,
        ExistingPathStatus::Missing => candidate,
        ExistingPathStatus::Unresolvable => {
            *incomplete = true;
            candidate
        }
    };
    let entry = inherit_path_provenance(
        ProjectPathEntry::resolved(path, &resolved, false),
        parent_provenance,
    );
    if !read_policy.allows_location(&entry) {
        *incomplete = true;
        warnings.push(format!(
            "ignored package path outside authorized read roots: {}",
            entry.path.display()
        ));
        return None;
    }
    Some(entry)
}

fn add_package_unit_candidate(
    metadata: &mut PackageMetadata,
    name: String,
    entry: ProjectPathEntry,
) {
    add_unit_candidate(&mut metadata.units, name.clone(), entry.path.clone());
    add_unique_project_path_entry(metadata.unit_entries.entry(name).or_default(), entry);
}

fn declared_package_name(source: &str) -> Option<String> {
    let tokens = lex_pascal(source);
    tokens.windows(2).find_map(|tokens| {
        let [PascalToken::Word(keyword), PascalToken::Word(name)] = tokens else {
            return None;
        };
        keyword
            .eq_ignore_ascii_case("package")
            .then(|| canonical_package_name(name))
            .filter(|name| !name.is_empty())
    })
}

pub(super) fn parse_explicit_unit_paths(source: &str) -> Vec<(String, String)> {
    parse_unit_membership(source).explicit_paths
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    #[cfg(unix)]
    #[test]
    fn package_metadata_rejects_a_symlinked_descriptor() {
        let temp = tempfile::tempdir().expect("temporary package directory");
        let outside = temp.path().join("outside.dpk");
        let descriptor = temp.path().join("Package.dpk");
        fs::write(&outside, "package Package; end.\n").expect("package descriptor");
        symlink(&outside, &descriptor).expect("descriptor symlink");
        let root = temp.path().to_path_buf();
        let read_policy = ReadPolicy::new(
            std::slice::from_ref(&root),
            &[],
            &[],
            &EffectiveOverrides::default(),
        );
        let entry = ProjectPathEntry {
            path: descriptor.clone(),
            provenance: ProjectPathProvenance::Configured,
        };

        let error = read_package_metadata(
            &descriptor,
            &ProjectOptions::default(),
            &EffectiveOverrides::default(),
            &read_policy,
            &entry,
        )
        .expect_err("symlinked package descriptors must not be opened");
        assert!(
            error.contains("regular file")
                || error.contains("symlink")
                || error.contains("authorized"),
            "unexpected package descriptor error: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn package_metadata_rejects_relative_units_outside_a_mapped_descriptor_root() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = temp.path();
        let sdk = root.join("sdk");
        let outside = root.join("outside");
        let descriptor = sdk.join("Package.dpk");
        fs::create_dir_all(&sdk).expect("mapped package directory");
        fs::create_dir_all(&outside).expect("outside directory");
        fs::write(
            &descriptor,
            "package Package; contains Provider in '../outside/Provider.pas'; end.",
        )
        .expect("package descriptor");
        fs::write(
            outside.join("Provider.pas"),
            "unit Provider; interface implementation end.",
        )
        .expect("outside package unit");
        let overrides = EffectiveOverrides {
            path_mappings: vec![crate::delphi_overrides::PathMapping {
                from: "c:/sdk".to_string(),
                to: sdk.clone(),
                config_file: root.join(".delphi-tools.local.toml"),
            }],
            ..EffectiveOverrides::default()
        };
        let read_policy = ReadPolicy::new(&[root.to_path_buf()], &[], &[], &overrides);
        let entry = ProjectPathEntry {
            path: descriptor.clone(),
            provenance: ProjectPathProvenance::Mapped { root: sdk },
        };

        let metadata = read_package_metadata(
            &descriptor,
            &ProjectOptions::default(),
            &overrides,
            &read_policy,
            &entry,
        )
        .expect("read package metadata");

        assert!(
            !metadata
                .units
                .values()
                .flatten()
                .any(|path| path == &outside.join("Provider.pas")),
            "package metadata exposed a unit outside its mapped descriptor root: {metadata:?}"
        );
    }
}

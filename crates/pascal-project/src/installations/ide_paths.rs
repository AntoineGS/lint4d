#[cfg(test)]
mod tests {
    use super::super::{evaluate_ide_paths, load_installation};
    use crate::delphi_overrides::EffectiveOverrides;
    use crate::installation_config::ResolvedInstallation;
    use crate::{ProjectReadTracker, ProjectWorkBudget, ReadPolicy};
    use std::fs;
    use std::path::Path;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn ide_paths_preserve_platform_order_and_relocate_original_bdslib() {
        let temp = tempfile::tempdir().unwrap();
        let sdk = temp.path().join("sdk");
        let ide = temp.path().join("ide");
        write(&sdk.join("bin/rsvars.bat"), "SET BDS=C:\\Original\r\n");
        write(
            &ide.join("environment.proj"),
            "<Project><PropertyGroup><BDSLIB>C:\\Original\\lib</BDSLIB></PropertyGroup></Project>",
        );
        write(
            &ide.join("EnvOptions.proj"),
            r#"<Project>
  <PropertyGroup Condition="'$(Platform)'=='Win32'"><DelphiLibraryPath>$(BDS)\wrong-platform</DelphiLibraryPath></PropertyGroup>
  <PropertyGroup Condition="'$(Platform)'=='Linux64'"><DelphiLibraryPath>$(BDSLIB)\base</DelphiLibraryPath><DelphiBrowsingPath>$(BDS)\source\rtl</DelphiBrowsingPath></PropertyGroup>
  <PropertyGroup Condition="'$(Platform)'=='Linux64'"><DelphiLibraryPath>$(BDS)\addon;$(DelphiLibraryPath)</DelphiLibraryPath></PropertyGroup>
</Project>"#,
        );

        let overrides = EffectiveOverrides {
            properties: [
                ("bds".to_owned(), sdk.to_string_lossy().into_owned()),
                ("appdata".to_owned(), ide.to_string_lossy().into_owned()),
            ]
            .into_iter()
            .collect(),
            ..EffectiveOverrides::default()
        };
        let profile = ResolvedInstallation {
            id: "37.0".to_owned(),
            overrides,
        };
        let roots = [sdk.clone(), ide.clone()];
        let policy =
            ReadPolicy::new_with_installation_roots(&[], &[], &[], &profile.overrides, &roots);
        let mut tracker = ProjectReadTracker::default();

        let environment = load_installation(&profile, "Debug", "Linux64", &mut tracker, &policy)
            .expect("installation environment loads");
        let paths = evaluate_ide_paths(
            &environment,
            &profile,
            "Debug",
            "Linux64",
            &mut tracker,
            &policy,
        )
        .expect("IDE paths evaluate");

        assert_eq!(
            paths
                .library
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>(),
            [sdk.join("addon"), sdk.join("lib/base")]
        );
        assert_eq!(
            paths
                .browsing
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>(),
            [sdk.join("source/rtl")]
        );
        assert!(
            paths
                .library
                .iter()
                .all(|entry| !entry.path.ends_with("wrong-platform"))
        );
        assert_eq!(
            environment.properties.get("bdslib"),
            Some(&sdk.join("lib").to_string_lossy().into_owned())
        );
    }

    #[test]
    fn legacy_keys_and_explicit_envoptions_locator_are_scoped_to_win32() {
        let temp = tempfile::tempdir().unwrap();
        let sdk = temp.path().join("local sdk");
        let options = temp.path().join("external IDE options.proj");
        write(
            &options,
            r#"<Project><PropertyGroup Condition="'$(Platform)'=='Win32'"><Win32LibraryPath>$(BDS)\lib;</Win32LibraryPath><Win32BrowsingPath>$(BDS)\source</Win32BrowsingPath><DelphiNamespaceSearchPath>System;Vcl</DelphiNamespaceSearchPath></PropertyGroup></Project>"#,
        );
        let profile = ResolvedInstallation {
            id: "7.0".to_owned(),
            overrides: EffectiveOverrides {
                properties: [
                    ("bds".to_owned(), sdk.to_string_lossy().into_owned()),
                    (
                        "envoptions".to_owned(),
                        options.to_string_lossy().into_owned(),
                    ),
                ]
                .into_iter()
                .collect(),
                ..EffectiveOverrides::default()
            },
        };
        let roots = [sdk.clone(), options.clone()];
        let policy =
            ReadPolicy::new_with_installation_roots(&[], &[], &[], &profile.overrides, &roots);
        let mut tracker = ProjectReadTracker::default();
        let environment =
            load_installation(&profile, "Debug", "Win32", &mut tracker, &policy).unwrap();
        let paths = evaluate_ide_paths(
            &environment,
            &profile,
            "Debug",
            "Win32",
            &mut tracker,
            &policy,
        )
        .unwrap();
        assert_eq!(paths.library[0].path, sdk.join("lib"));
        assert_eq!(paths.browsing[0].path, sdk.join("source"));
        assert_eq!(paths.namespaces, ["System", "Vcl"]);

        let other_platform = evaluate_ide_paths(
            &environment,
            &profile,
            "Debug",
            "Linux64",
            &mut tracker,
            &policy,
        )
        .unwrap();
        assert!(other_platform.library.is_empty());
    }

    #[test]
    fn missing_optional_environment_file_is_not_a_load_failure() {
        let temp = tempfile::tempdir().unwrap();
        let sdk = temp.path().join("sdk");
        let ide = temp.path().join("ide");
        let profile = ResolvedInstallation {
            id: "37.0".to_owned(),
            overrides: EffectiveOverrides {
                properties: [
                    ("bds".to_owned(), sdk.to_string_lossy().into_owned()),
                    ("appdata".to_owned(), ide.to_string_lossy().into_owned()),
                ]
                .into_iter()
                .collect(),
                ..EffectiveOverrides::default()
            },
        };
        let roots = [sdk, ide.clone()];
        let policy =
            ReadPolicy::new_with_installation_roots(&[], &[], &[], &profile.overrides, &roots);
        let mut tracker = ProjectReadTracker::default();
        let environment = load_installation(&profile, "Debug", "Win32", &mut tracker, &policy)
            .expect("missing optional environment.proj is a default layer");
        assert!(
            environment
                .warnings
                .iter()
                .any(|warning| warning.contains("rsvars"))
        );
        assert!(
            environment
                .metadata_observations
                .iter()
                .any(|observation| observation.path().ends_with("environment.proj"))
        );

        write(&ide.join("environment.proj"), "<Project><PropertyGroup>");
        let mut tracker = ProjectReadTracker::default();
        let malformed =
            load_installation(&profile, "Debug", "Win32", &mut tracker, &policy).unwrap();
        assert!(
            malformed
                .warnings
                .iter()
                .any(|warning| warning.contains("malformed environment settings"))
        );
    }

    #[test]
    fn installation_loading_propagates_budget_cancellation() {
        struct Cancelled;
        impl ProjectWorkBudget for Cancelled {
            fn check_cancelled(&self) -> Result<(), String> {
                Err("request cancelled".to_owned())
            }
            fn charge_path_visits(&self, _: usize) -> Result<(), String> {
                Ok(())
            }
            fn ensure_file_read_fits(&self, _: usize) -> Result<(), String> {
                Ok(())
            }
            fn charge_file_bytes(&self, _: usize) -> Result<(), String> {
                Ok(())
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let profile = ResolvedInstallation {
            id: "37.0".to_owned(),
            overrides: EffectiveOverrides {
                properties: [("bds".to_owned(), temp.path().to_string_lossy().into_owned())]
                    .into_iter()
                    .collect(),
                ..EffectiveOverrides::default()
            },
        };
        let policy = ReadPolicy::new_with_installation_roots(
            &[],
            &[],
            &[],
            &profile.overrides,
            &[temp.path().to_path_buf()],
        );
        let budget = Cancelled;
        let mut tracker = ProjectReadTracker::with_budget(Some(&budget), &[]);
        assert_eq!(
            load_installation(&profile, "Debug", "Win32", &mut tracker, &policy).unwrap_err(),
            "request cancelled"
        );
    }
}
use super::{
    InstallationEnvironment, PropertyMap, relocate_environment, resolve_path_with_inferred,
};
use crate::delphi_overrides::EffectiveOverrides;
use crate::installation_config::ResolvedInstallation;
use crate::{
    MetadataObservation, ProjectBuilder, ProjectOptions, ProjectPathEntry, ProjectPathProvenance,
    ProjectReadTracker, ReadPolicy,
};
use std::fs;
use std::path::{Path, PathBuf};

const MAX_INSTALLATION_XML_BYTES: u64 = 4 * 1024 * 1024;
const MAX_RS_VARS_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub(crate) struct IdePaths {
    pub library: Vec<ProjectPathEntry>,
    pub browsing: Vec<ProjectPathEntry>,
    pub debug_dcu: Vec<ProjectPathEntry>,
    pub namespaces: Vec<String>,
}

pub(crate) fn load_installation(
    profile: &ResolvedInstallation,
    config: &str,
    platform: &str,
    tracker: &mut ProjectReadTracker<'_>,
    policy: &ReadPolicy,
) -> Result<InstallationEnvironment, String> {
    let bds = property_path(&profile.overrides, "bds", Path::new("/"));
    let appdata = property_path(&profile.overrides, "appdata", Path::new("/"));
    let mut warnings = Vec::new();
    let mut observations = Vec::new();
    let mut metadata_files = Vec::new();
    let mut imported = PropertyMap::new();
    let mut original_bds = None;

    if let Some(root) = bds.as_ref() {
        let rsvars = root.join("bin/rsvars.bat");
        if let Some(contents) = read_installation_file(
            &rsvars,
            false,
            MAX_RS_VARS_BYTES,
            policy,
            tracker,
            &mut observations,
            &mut metadata_files,
            &mut warnings,
        )? {
            match super::parse_rsvars(&contents, &PropertyMap::new()) {
                Ok(properties) => {
                    original_bds = properties.get("bds").cloned();
                    imported = properties;
                }
                Err(error) => {
                    warnings.push(format!("could not parse {}: {error}", rsvars.display()))
                }
            }
        }
    } else {
        warnings.push("selected Delphi installation has no configured BDS root".to_owned());
    }

    if let Some(original_bds) = original_bds.as_ref() {
        imported.insert("bds".to_owned(), original_bds.clone());
    }

    if let Some(path) = locator_path(
        &profile.overrides,
        "environmentsettings",
        appdata.as_deref(),
        "environment.proj",
    ) {
        if let Some(contents) = read_installation_file(
            &path,
            true,
            MAX_INSTALLATION_XML_BYTES,
            policy,
            tracker,
            &mut observations,
            &mut metadata_files,
            &mut warnings,
        )? {
            let mut builder =
                installation_builder(profile, config, platform, policy, &path, warnings.clone());
            builder.seed_installation_properties(&imported);
            match builder.process_installation_file(
                &contents,
                &path,
                tracker,
                &ProjectPathProvenance::Configured,
            ) {
                Ok(()) => {
                    imported = builder
                        .properties
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    if let Some(original_bds) = original_bds.as_ref() {
                        imported.insert("bds".to_owned(), original_bds.clone());
                    }
                    warnings = builder.warnings;
                }
                Err(error) => {
                    warnings.push(format!(
                        "malformed environment settings {}: {error}",
                        path.display()
                    ));
                    // A present but malformed file cannot safely supply any of its properties.
                    observations.push(MetadataObservation::Stat { path: path.clone() });
                }
            }
        }
    }

    let relocated = relocate_environment(&imported, &profile.overrides)?;
    let mut properties = relocated.properties;
    if let Some(root) = bds.as_ref() {
        properties
            .entry("bds".to_owned())
            .or_insert_with(|| root.to_string_lossy().into_owned());
    }
    derive_roots(&mut properties);
    let mut read_roots = relocated.read_roots;
    for path in [bds, appdata].into_iter().flatten() {
        if !read_roots.contains(&path) {
            read_roots.push(path);
        }
    }
    Ok(InstallationEnvironment {
        properties,
        read_roots,
        inferred_mappings: relocated.inferred_mappings,
        metadata_files,
        metadata_observations: observations,
        path_issues: Vec::new(),
        warnings,
    })
}

pub(crate) fn evaluate_ide_paths(
    environment: &InstallationEnvironment,
    profile: &ResolvedInstallation,
    config: &str,
    platform: &str,
    tracker: &mut ProjectReadTracker<'_>,
    policy: &ReadPolicy,
) -> Result<IdePaths, String> {
    let appdata = property_path(&profile.overrides, "appdata", Path::new("/"));
    let Some(path) = locator_path(
        &profile.overrides,
        "envoptions",
        appdata.as_deref(),
        "EnvOptions.proj",
    ) else {
        return Ok(IdePaths::default());
    };
    let mut observations = Vec::new();
    let mut files = Vec::new();
    let mut warnings = Vec::new();
    let Some(contents) = read_installation_file(
        &path,
        false,
        MAX_INSTALLATION_XML_BYTES,
        policy,
        tracker,
        &mut observations,
        &mut files,
        &mut warnings,
    )?
    else {
        return Ok(IdePaths::default());
    };

    let options = ProjectOptions {
        build_config: Some(config.to_owned()),
        platform: Some(platform.to_owned()),
        ..ProjectOptions::default()
    };
    let mut builder = ProjectBuilder::new(
        &options,
        &profile.overrides,
        Vec::new(),
        path.parent()
            .unwrap_or_else(|| Path::new("/"))
            .to_path_buf(),
        policy.clone(),
    );
    builder.seed_installation_properties(
        &environment
            .properties
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    );
    builder.process_installation_file(
        &contents,
        &path,
        tracker,
        &ProjectPathProvenance::Configured,
    )?;
    let legacy = profile.id == "7.0" && platform.eq_ignore_ascii_case("Win32");
    let (library_key, browsing_key, debug_key) = if legacy {
        ("win32librarypath", "win32browsingpath", "win32debugdcupath")
    } else {
        (
            "delphilibrarypath",
            "delphibrowsingpath",
            "delphidebugdcupath",
        )
    };
    Ok(IdePaths {
        library: property_paths(&builder, library_key, profile, environment, &mut warnings),
        browsing: property_paths(&builder, browsing_key, profile, environment, &mut warnings),
        debug_dcu: property_paths(&builder, debug_key, profile, environment, &mut warnings),
        namespaces: builder
            .property_list_with_provenance("delphinamespacesearchpath")
            .into_iter()
            .map(|(value, _)| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .collect(),
    })
}

fn installation_builder(
    profile: &ResolvedInstallation,
    config: &str,
    platform: &str,
    policy: &ReadPolicy,
    file: &Path,
    warnings: Vec<String>,
) -> ProjectBuilder {
    let options = ProjectOptions {
        build_config: Some(config.to_owned()),
        platform: Some(platform.to_owned()),
        ..ProjectOptions::default()
    };
    ProjectBuilder::new(
        &options,
        &profile.overrides,
        warnings,
        file.parent()
            .unwrap_or_else(|| Path::new("/"))
            .to_path_buf(),
        policy.clone(),
    )
}

fn property_path(overrides: &EffectiveOverrides, name: &str, base: &Path) -> Option<PathBuf> {
    let value = overrides.properties.get(name)?;
    overrides
        .resolve_path(value, base)
        .ok()
        .map(|resolved| resolved.path)
}

fn locator_path(
    overrides: &EffectiveOverrides,
    name: &str,
    appdata: Option<&Path>,
    filename: &str,
) -> Option<PathBuf> {
    overrides
        .properties
        .get(name)
        .and_then(|value| {
            overrides
                .resolve_path(value, Path::new("/"))
                .ok()
                .map(|path| path.path)
        })
        .or_else(|| appdata.map(|root| root.join(filename)))
}

#[allow(clippy::too_many_arguments)]
fn read_installation_file(
    path: &Path,
    optional_absence: bool,
    limit: u64,
    policy: &ReadPolicy,
    tracker: &mut ProjectReadTracker<'_>,
    observations: &mut Vec<MetadataObservation>,
    metadata_files: &mut Vec<PathBuf>,
    warnings: &mut Vec<String>,
) -> Result<Option<String>, String> {
    if let Some(budget) = tracker.work_budget {
        budget.check_cancelled()?;
        budget.charge_path_visits(1)?;
    }
    let entry = policy
        .entry_for_path(path)
        .unwrap_or_else(|| ProjectPathEntry {
            path: path.to_path_buf(),
            provenance: ProjectPathProvenance::Configured,
        });
    tracker.record_metadata_path(path.to_path_buf());
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            observations.push(MetadataObservation::Stat {
                path: path.to_path_buf(),
            });
            tracker.record_metadata_observation(MetadataObservation::Stat {
                path: path.to_path_buf(),
            });
            if !optional_absence {
                warnings.push(format!(
                    "installation metadata is absent: {}",
                    path.display()
                ));
            }
            return Ok(None);
        }
        Err(error) => {
            warnings.push(format!(
                "could not inspect installation metadata {}: {error}",
                path.display()
            ));
            return Ok(None);
        }
        Ok(_) => {}
    }
    match crate::read_payload_with_tracker(policy, &entry, limit, tracker) {
        Ok((contents, observation)) => {
            metadata_files.push(path.to_path_buf());
            observations.push(observation.clone());
            tracker.record_metadata_observation(observation);
            Ok(Some(contents))
        }
        Err(error) => {
            if error == "request cancelled" || error.contains("budget") {
                return Err(error);
            }
            warnings.push(format!(
                "could not read installation metadata {}: {error}",
                path.display()
            ));
            observations.push(MetadataObservation::Stat {
                path: path.to_path_buf(),
            });
            tracker.record_metadata_observation(MetadataObservation::Stat {
                path: path.to_path_buf(),
            });
            Ok(None)
        }
    }
}

fn derive_roots(properties: &mut PropertyMap) {
    if let Some(bds) = properties.get("bds").cloned() {
        for (name, suffix) in [
            ("bdslib", "lib"),
            ("bdsinclude", "include"),
            ("bdsbin", "bin"),
        ] {
            properties
                .entry(name.to_owned())
                .or_insert_with(|| Path::new(&bds).join(suffix).to_string_lossy().into_owned());
        }
    }
}

fn property_paths(
    builder: &ProjectBuilder,
    key: &str,
    profile: &ResolvedInstallation,
    environment: &InstallationEnvironment,
    warnings: &mut Vec<String>,
) -> Vec<ProjectPathEntry> {
    let mut entries = Vec::new();
    for (raw, provenance) in builder.property_list_with_provenance(key) {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let normalized;
        let raw = if !is_windows_absolute(raw) {
            normalized = raw.replace('\\', "/");
            normalized.as_str()
        } else {
            raw
        };
        match resolve_path_with_inferred(
            &profile.overrides,
            &environment.inferred_mappings,
            raw,
            Path::new("/"),
        ) {
            Ok(resolved) => {
                let entry = ProjectPathEntry::resolved(
                    resolved.path.clone(),
                    &resolved,
                    provenance == ProjectPathProvenance::Configured,
                );
                if entries
                    .iter()
                    .all(|existing: &ProjectPathEntry| existing.path != entry.path)
                {
                    entries.push(entry);
                }
            }
            Err(error) => warnings.push(format!("could not resolve {key} entry `{raw}`: {error}")),
        }
    }
    entries
}

fn is_windows_absolute(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    (bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\'))
        || raw.starts_with("\\\\")
        || raw.starts_with("//")
}

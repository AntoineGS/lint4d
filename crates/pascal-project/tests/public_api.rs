use pascal_project::delphi_overrides::OverrideSession;
use pascal_project::{
    MetadataObservation, ProjectContext, ProjectOptions, ProjectPathProvenance, ProjectSelections,
    discover_with_selections_and_observations_with_cancel_and_overrides,
};
use std::fs;
use std::sync::atomic::AtomicBool;
use tempfile::tempdir;

fn write(path: &std::path::Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("fixture directory");
    }
    fs::write(path, contents).expect("fixture file");
}

#[test]
fn public_api_discovers_a_standalone_pascal_source() {
    let directory = tempdir().expect("temporary directory");
    let source = directory.path().join("main.pas");
    fs::write(&source, "unit Main; interface implementation end.").expect("source file");
    let overrides = OverrideSession::new(None);

    let context = ProjectContext::discover_with_overrides(
        &source,
        &[directory.path().to_path_buf()],
        &ProjectOptions::default(),
        &overrides,
    )
    .expect("standalone discovery");

    assert!(context.discovery_complete);
    assert_eq!(context.main_source, None);
    assert_eq!(context.search_paths, vec![directory.path().to_path_buf()]);
}

#[test]
fn public_api_finds_configuration_directories_from_a_workspace_root() {
    let directory = tempdir().expect("temporary directory");
    let source_directory = directory.path().join("src");
    fs::create_dir(&source_directory).expect("source directory");
    let source = source_directory.join("main.pas");

    let directories =
        pascal_project::config_directories(&source, None, &[directory.path().to_path_buf()])
            .expect("configuration directories");

    assert_eq!(directories, vec![directory.path().to_path_buf()]);
}

#[test]
fn public_api_resolves_dproj_optset_configuration_and_metadata() {
    let directory = tempdir().expect("temporary workspace");
    let root = directory.path();
    let project_directory = root.join("App");
    let main = project_directory.join("App.dpr");
    let project = project_directory.join("App.dproj");
    let option_set = project_directory.join("Debug.optset");
    let include_path = project_directory.join("Include");
    let unit_path = project_directory.join("Units");

    write(&main, "program App; begin end.");
    fs::create_dir_all(&include_path).expect("include path");
    fs::create_dir_all(&unit_path).expect("unit path");
    write(
        &project,
        r#"<Project>
  <PropertyGroup>
    <MainSource>App.dpr</MainSource>
    <Config Condition="'$(Config)'==''">Release</Config>
  </PropertyGroup>
  <Import Project="Debug.optset" />
</Project>"#,
    );
    write(
        &option_set,
        r#"<Project>
  <PropertyGroup Condition="('$(Config)'=='Debug') And ('$(Platform)'=='Win64')">
    <DCC_Define>PUBLIC_DEFINE;$(DCC_Define)</DCC_Define>
    <DCC_IncludePath>Include;$(DCC_IncludePath)</DCC_IncludePath>
    <DCC_UnitSearchPath>Units;$(DCC_UnitSearchPath)</DCC_UnitSearchPath>
    <DCC_UnitAlias>LegacyUnit=ModernUnit;$(DCC_UnitAlias)</DCC_UnitAlias>
  </PropertyGroup>
</Project>"#,
    );

    let options = ProjectOptions {
        build_config: Some("Debug".to_string()),
        platform: Some("Win64".to_string()),
        ..ProjectOptions::default()
    };
    let overrides = OverrideSession::new(None);
    let context =
        ProjectContext::discover_with_overrides(&main, &[root.to_path_buf()], &options, &overrides)
            .expect("project discovery");

    assert_eq!(context.project_file, Some(project.clone()));
    assert_eq!(context.main_source, Some(main));
    assert_eq!(context.config.as_deref(), Some("Debug"));
    assert_eq!(context.platform.as_deref(), Some("Win64"));
    assert!(context.defines.contains(&"PUBLIC_DEFINE".to_string()));
    assert!(context.include_paths.contains(&include_path));
    assert!(context.search_paths.contains(&unit_path));
    assert_eq!(
        context.unit_aliases.get("LegacyUnit"),
        Some(&"ModernUnit".to_string())
    );
    assert!(context.metadata_files.contains(&project));
    assert!(context.metadata_files.contains(&option_set));
    for expected in [&project, &option_set] {
        assert!(
            context.metadata_observations.iter().any(|observation| {
                observation.path() == expected
                    && matches!(observation, MetadataObservation::Payload { .. })
            }),
            "missing payload observation for {expected:?}: {:?}",
            context.metadata_observations
        );
    }
    assert!(
        context
            .search_path_entries
            .iter()
            .any(|entry| entry.path == unit_path
                && entry.provenance == ProjectPathProvenance::LegacyNative)
    );
}

#[test]
fn public_api_rejects_a_precancelled_discovery() {
    let directory = tempdir().expect("temporary workspace");
    let source = directory.path().join("main.pas");
    write(&source, "unit Main; interface implementation end.");
    let cancel = AtomicBool::new(true);
    let overrides = OverrideSession::new(None);

    let error = discover_with_selections_and_observations_with_cancel_and_overrides(
        &source,
        &[directory.path().to_path_buf()],
        &ProjectOptions::default(),
        &ProjectSelections::new(),
        &overrides,
        &[],
        &cancel,
    )
    .expect_err("pre-cancelled discovery must fail before filesystem work");

    assert_eq!(error, "request cancelled");
}

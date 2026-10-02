use std::{
    fs,
    path::{Path, PathBuf},
};

use pascal_project::{
    CompilerVersion, InstallationSelection, ProjectContext, ProjectOptions,
    delphi_overrides::OverrideSession,
};

fn local_roots() -> (PathBuf, PathBuf) {
    let delphi = PathBuf::from(
        std::env::var_os("PASCAL_TEST_DELPHI_ROOT").expect("PASCAL_TEST_DELPHI_ROOT must be set"),
    );
    let project = PathBuf::from(
        std::env::var_os("PASCAL_TEST_PROJECT_ROOT").expect("PASCAL_TEST_PROJECT_ROOT must be set"),
    );
    (delphi, project)
}

fn quote(path: &Path) -> String {
    toml::Value::String(path.to_string_lossy().into_owned()).to_string()
}

fn discover_fixture(
    delphi: &Path,
    multidev: &Path,
    version: &str,
    install: &str,
    ide: &str,
) -> (tempfile::TempDir, ProjectContext) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::write(root.join("Probe.dpr"), "program Probe; begin end.").unwrap();
    fs::write(
        root.join("Probe.dproj"),
        "<Project><PropertyGroup><MainSource>Probe.dpr</MainSource><Config>Debug</Config><Platform>Win32</Platform></PropertyGroup></Project>",
    ).unwrap();
    fs::write(
        root.join(".delphi-tools.local.toml"),
        format!(
            "[properties]\nMULTIDEV={}\n[installations.\"{version}\".properties]\nBDS={}\nAPPDATA={}\nENVOPTIONS={}\n[projects.\"Probe.dproj\"]\ninstallation='{version}'\n",
            quote(multidev), quote(&delphi.join(install)), quote(&delphi.join(ide)),
            quote(&delphi.join(ide).join("EnvOptions.proj")),
        ),
    ).unwrap();
    let context = ProjectContext::discover_with_overrides(
        &root.join("Probe.dpr"),
        &[root.to_path_buf()],
        &ProjectOptions::default(),
        &OverrideSession::new(None),
    )
    .unwrap();
    (temp, context)
}

#[test]
#[ignore = "requires explicitly supplied local Delphi installations"]
fn local_delphi_installations_resolve_configured_roots() {
    let (delphi, multidev) = local_roots();
    assert!(
        multidev
            .join("Projects/ChainDriveAPI/ChainDriveAPI.dproj")
            .is_file()
    );

    let mut failures = Vec::new();
    for (version, compiler, install, ide) in [
        ("7.0", 21, "RAD Studio/7.0", "AppData/CodeGear/BDS/7.0"),
        (
            "10.0",
            24,
            "RAD Studio/10.0",
            "AppData/Embarcadero/BDS/10.0",
        ),
        ("37.0", 37, "Studio/37.0", "AppData/Embarcadero/BDS/37.0"),
    ] {
        let sdk = delphi.join(install);
        let ide_root = delphi.join(ide);
        assert!(
            sdk.join("bin/rsvars.bat").is_file(),
            "missing {version} rsvars.bat"
        );
        assert!(
            ide_root.join("EnvOptions.proj").is_file(),
            "missing {version} EnvOptions.proj"
        );

        let (_fixture, context) = discover_fixture(&delphi, &multidev, version, install, ide);
        if !matches!(&context.installation_selection,
            Some(InstallationSelection::Selected { id, .. }) if id == version)
        {
            failures.push(format!(
                "{version}: unexpected selection {:?}",
                context.installation_selection
            ));
        }
        if context.conditional_context.compiler_version != Some(CompilerVersion::new(compiler, 0)) {
            failures.push(format!(
                "{version}: compiler version {:?}",
                context.conditional_context.compiler_version
            ));
        }
        eprintln!(
            "Delphi {version}: selection={:?}; metadata={:?}; issues={:?}; warning_count={}",
            context.installation_selection,
            context.metadata_files,
            context.path_issues,
            context.warnings.len()
        );
        if !context
            .search_paths
            .iter()
            .any(|path| path.starts_with(&sdk) && path.is_dir())
        {
            failures.push(format!(
                "{version}: no existing SDK search path; selected paths {:?}",
                context.search_paths
            ));
        }
        if !context
            .browsing_path_entries
            .iter()
            .any(|entry| entry.path.starts_with(&sdk) && entry.path.is_dir())
        {
            failures.push(format!(
                "{version}: no existing SDK browsing path; entries {:?}",
                context.browsing_path_entries
            ));
        }
        eprintln!(
            "Delphi {version}: selected {:?}; existing search paths: {:?}; browsing paths: {:?}",
            context.installation_selection,
            context
                .search_paths
                .iter()
                .filter(|p| p.starts_with(&sdk) && p.is_dir())
                .collect::<Vec<_>>(),
            context
                .browsing_path_entries
                .iter()
                .filter(|p| p.path.starts_with(&sdk) && p.path.is_dir())
                .map(|p| &p.path)
                .collect::<Vec<_>>()
        );
    }

    // The 23.0 tree is known to be partial. Discovery must retain scoped
    // availability information rather than treating that profile as fatal.
    let (_fixture, partial) = discover_fixture(
        &delphi,
        &multidev,
        "23.0",
        "Studio/23.0",
        "AppData/Embarcadero/BDS/23.0",
    );
    if !matches!(&partial.installation_selection,
        Some(InstallationSelection::Selected { id, .. }) if id == "23.0")
    {
        failures.push(format!(
            "23.0: unexpected selection {:?}",
            partial.installation_selection
        ));
    }
    if partial.discovery_complete && partial.path_issues.is_empty() && partial.warnings.is_empty() {
        failures.push("23.0: no scoped partial-availability signal".to_string());
    }
    eprintln!(
        "Delphi 23.0 partial profile: complete={}, issues={:?}, warning_count={}, first_warnings={:?}",
        partial.discovery_complete,
        partial.path_issues,
        partial.warnings.len(),
        partial.warnings.iter().take(3).collect::<Vec<_>>()
    );

    // Validate ChainDriveAPI as a separate real-project discovery operation,
    // using only profile 37.0 (third-party paths remain independently scoped).
    let project_file = multidev.join("Projects/ChainDriveAPI/ChainDriveAPI.dproj");
    let chain_config = tempfile::tempdir().unwrap();
    let chain_config_path = chain_config.path().join("local.toml");
    fs::write(&chain_config_path, format!(
        "[installations.\"37.0\".properties]\nBDS={}\nAPPDATA={}\nENVOPTIONS={}\n[projects.\"ChainDriveAPI.dproj\"]\ninstallation='37.0'\n",
        quote(&delphi.join("Studio/37.0")),
        quote(&delphi.join("AppData/Embarcadero/BDS/37.0")),
        quote(&delphi.join("AppData/Embarcadero/BDS/37.0/EnvOptions.proj")),
    )).unwrap();
    let chain = ProjectContext::discover_with_overrides(
        &project_file,
        std::slice::from_ref(&multidev),
        &ProjectOptions {
            project_file: Some(project_file.clone()),
            installation_selections: [(project_file.clone(), "37.0".to_string())]
                .into_iter()
                .collect(),
            ..ProjectOptions::default()
        },
        &OverrideSession::new(Some(chain_config_path)),
    )
    .unwrap();
    if !matches!(&chain.installation_selection,
        Some(InstallationSelection::Selected { id, .. }) if id == "37.0")
    {
        failures.push(format!(
            "ChainDriveAPI: expected profile 37.0, got {:?}",
            chain.installation_selection
        ));
    }
    let third_party_missing = chain
        .path_issues
        .iter()
        .filter(|issue| {
            issue
                .path
                .as_ref()
                .is_some_and(|path| path.starts_with(delphi.join("Dependencies")))
        })
        .collect::<Vec<_>>();
    eprintln!(
        "ChainDriveAPI 37.0: selected {:?}; issues={:?}; missing third-party roots={:?}",
        chain.installation_selection, chain.path_issues, third_party_missing
    );
    assert!(
        failures.is_empty(),
        "real-installation checks failed:\n{}",
        failures.join("\n")
    );
}

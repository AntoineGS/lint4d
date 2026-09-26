use std::{fs, path::Path};

use pascal_project::delphi_overrides::{EffectiveOverrides, OverrideSession};
use pascal_project::installations::InstallationEvidence;
use pascal_project::{
    InstallationOrigin, InstallationSelection, ProjectContext, ProjectOptions, ProjectPathEntry,
    ProjectPathProvenance, ReadPolicy,
};

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

#[test]
fn installation_read_roots_authorize_only_the_selected_local_tree() {
    let temp = tempfile::tempdir().unwrap();
    let sdk = temp.path().join("sdk");
    let unrelated = temp.path().join("other-sdk");
    fs::create_dir_all(&sdk).unwrap();
    fs::create_dir_all(&unrelated).unwrap();
    let policy = ReadPolicy::new_with_installation_roots(
        &[],
        &[],
        &[],
        &EffectiveOverrides::default(),
        std::slice::from_ref(&sdk),
    );
    let selected = ProjectPathEntry {
        path: sdk.join("bin/rsvars.bat"),
        provenance: ProjectPathProvenance::Configured,
    };
    let not_selected = ProjectPathEntry {
        path: unrelated.join("bin/rsvars.bat"),
        provenance: ProjectPathProvenance::Configured,
    };
    assert!(policy.allows_location(&selected));
    assert!(!policy.allows_location(&not_selected));
}

#[test]
fn project_shared_properties_override_user_profile_properties() {
    let temp = tempfile::tempdir().unwrap();
    let user = temp.path().join("config.toml");
    let project = temp.path().join("repo/app/App.dproj");
    write(
        &user,
        "[installations.\"37.0\".properties]\nBDS = '/user/sdk'\n",
    );
    write(
        &project.with_file_name(".delphi-tools.local.toml"),
        "[properties]\nBDS = '/project/sdk'\n",
    );
    let session = OverrideSession::new(Some(user));
    let config = session
        .configuration_for(Some(&temp.path().join("repo")), Some(&project))
        .unwrap();
    assert_eq!(
        config.profile("37.0").unwrap().overrides.properties["bds"],
        "/project/sdk"
    );
}

#[test]
fn profiles_inherit_shared_values_and_keep_independent_direct_appdata_roots() {
    let temp = tempfile::tempdir().unwrap();
    let user = temp.path().join("config.toml");
    write(
        &user,
        "[properties]\nShared = 'user'\n\
         [installations.\"7.0\".properties]\nBDS = '/sdk/7'\nAPPDATA = '/ide/7'\n\
         [installations.\"10.0\".properties]\nBDS = '/sdk/10'\nAPPDATA = '/ide/10'\n",
    );

    let config = OverrideSession::new(Some(user))
        .configuration_for(None, None)
        .unwrap();
    assert_eq!(config.installation_ids(), ["10.0", "7.0"]);

    let seven = config.profile("7.0").unwrap();
    let ten = config.profile("10.0").unwrap();
    assert_eq!(seven.overrides.properties["appdata"], "/ide/7");
    assert_eq!(ten.overrides.properties["appdata"], "/ide/10");
    assert_eq!(seven.overrides.properties["shared"], "user");
    assert_eq!(seven.overrides.properties["bds"], "/sdk/7");
}

#[test]
fn profiles_merge_properties_and_mapping_origins_by_layer_and_profile() {
    let temp = tempfile::tempdir().unwrap();
    let user = temp.path().join("config.toml");
    let workspace = temp.path().join("repo");
    write(
        &user,
        "[installations.\"37.0\".properties]\nBDS = '/user/sdk'\nUserOnly = 'yes'\n\
         [[installations.\"37.0\".path_mappings]]\nfrom = 'C:\\\\SDK'\nto = '/user/sdk'\n",
    );
    write(
        &workspace.join(".delphi-tools.local.toml"),
        "[installations.\"37.0\".properties]\nBDS = '/workspace/sdk'\n\
         [[installations.\"37.0\".path_mappings]]\nfrom = 'C:\\\\SDK'\nto = '/workspace/sdk'\n",
    );

    let config = OverrideSession::new(Some(user.clone()))
        .configuration_for(Some(&workspace), None)
        .unwrap();
    let resolved = config.profile("37.0").unwrap();
    assert_eq!(resolved.overrides.properties["bds"], "/workspace/sdk");
    assert_eq!(resolved.overrides.properties["useronly"], "yes");
    assert_eq!(
        resolved.overrides.property_origins["bds"],
        workspace.join(".delphi-tools.local.toml")
    );
    assert_eq!(
        resolved.overrides.path_mappings[0].to,
        Path::new("/workspace/sdk")
    );
    assert_eq!(
        resolved.overrides.path_mappings[0].config_file,
        workspace.join(".delphi-tools.local.toml")
    );
}

#[test]
fn project_installation_selectors_are_exact_and_later_layers_replace_them() {
    let temp = tempfile::tempdir().unwrap();
    let user = temp.path().join("config.toml");
    let workspace = temp.path().join("repo");
    let first = workspace.join("app/First.dproj");
    let second = workspace.join("app/Second.dproj");
    write(
        &user,
        "[projects.\"repo/app/First.dproj\"]\ninstallation = '7.0'\n\
         [projects.\"repo/app/Second.dproj\"]\ninstallation = '10.0'\n",
    );
    write(
        &workspace.join(".delphi-tools.local.toml"),
        "[projects.\"app/First.dproj\"]\ninstallation = '37.0'\n",
    );
    let config = OverrideSession::new(Some(user))
        .configuration_for(Some(&workspace), Some(&first))
        .unwrap();
    assert_eq!(config.configured_installation_for(&first), Some("37.0"));
    assert_eq!(config.configured_installation_for(&second), Some("10.0"));
    assert_eq!(
        config.configured_installation_for(&workspace.join("app/Other.dproj")),
        None
    );
}

#[cfg(unix)]
#[test]
fn project_selectors_keep_unix_backslash_filenames_distinct_from_directories() {
    let temp = tempfile::tempdir().unwrap();
    let user = temp.path().join("config.toml");
    write(
        &user,
        "[projects.\"app\\\\One.dproj\"]\ninstallation = 'literal-backslash'\n\
         [projects.\"app/One.dproj\"]\ninstallation = 'directory'\n",
    );
    let literal_backslash = temp.path().join("app\\One.dproj");
    let directory = temp.path().join("app/One.dproj");

    let config = OverrideSession::new(Some(user))
        .configuration_for(None, None)
        .unwrap();
    assert_eq!(
        config.configured_installation_for(&literal_backslash),
        Some("literal-backslash")
    );
    assert_eq!(
        config.configured_installation_for(&directory),
        Some("directory")
    );
}

#[cfg(unix)]
#[test]
fn project_selector_lookup_does_not_alias_invalid_utf8_to_replacement_character() {
    use std::os::unix::ffi::OsStringExt;

    let temp = tempfile::tempdir().unwrap();
    let user = temp.path().join("config.toml");
    write(
        &user,
        "[projects.\"App�.dproj\"]\ninstallation = 'valid-unicode'\n",
    );
    let invalid_utf8 = temp
        .path()
        .join(std::ffi::OsString::from_vec(b"App\xff.dproj".to_vec()));
    let valid_unicode = temp.path().join("App�.dproj");

    let config = OverrideSession::new(Some(user))
        .configuration_for(None, None)
        .unwrap();
    assert_eq!(
        config.configured_installation_for(&valid_unicode),
        Some("valid-unicode")
    );
    assert_eq!(config.configured_installation_for(&invalid_utf8), None);
}

#[test]
fn project_selector_rejects_empty_installation_ids() {
    let temp = tempfile::tempdir().unwrap();
    let user = temp.path().join("config.toml");
    write(&user, "[projects.\"App.dproj\"]\ninstallation = '  '\n");

    let error = OverrideSession::new(Some(user.clone()))
        .configuration_for(None, None)
        .expect_err("empty installation IDs must be rejected");
    assert!(error.contains(&user.display().to_string()));
    assert!(error.contains("installation"));
}

#[test]
fn invalid_installation_configuration_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    for (name, contents) in [
        ("unknown.toml", "[mystery]\nvalue = 'x'\n"),
        ("malformed.toml", "[properties\nBDS = '/broken'\n"),
        (
            "duplicate-properties.toml",
            "[properties]\nBDS = '/one'\nbds = '/two'\n",
        ),
        (
            "duplicate-selectors.toml",
            "[projects.\"app/../App.dproj\"]\ninstallation = '7.0'\n\
             [projects.\"App.dproj\"]\ninstallation = '10.0'\n",
        ),
        (
            "empty-id.toml",
            "[installations.\"\".properties]\nBDS = '/sdk'\n",
        ),
    ] {
        let file = temp.path().join(name);
        write(&file, contents);
        let error = OverrideSession::new(Some(file))
            .configuration_for(None, None)
            .expect_err("invalid configuration should fail");
        assert!(error.contains(name), "error should name its file: {error}");
    }
}

#[test]
fn profile_lookup_rejects_unknown_ids_and_legacy_files_stay_shared_only() {
    let temp = tempfile::tempdir().unwrap();
    let user = temp.path().join("config.toml");
    write(&user, "[properties]\nBDS = '/legacy'\n");
    let session = OverrideSession::new(Some(user));
    let config = session.configuration_for(None, None).unwrap();
    assert!(config.installation_ids().is_empty());
    assert!(config.profile("37.0").unwrap_err().contains("unknown"));
    assert_eq!(
        session.effective_for(None, None).unwrap().properties["bds"],
        "/legacy"
    );
}

#[test]
fn public_identity_selection_uses_compiler_facts_and_exact_project_overrides() {
    let temp = tempfile::tempdir().unwrap();
    let config_file = temp.path().join("config.toml");
    let project = temp.path().join("app/App.dproj");
    write(
        &config_file,
        "[installations.\"7.0\".properties]\nBDS = '/sdk/7'\n\
         [installations.\"10.0\".properties]\nBDS = '/sdk/10'\n",
    );
    let config = OverrideSession::new(Some(config_file))
        .configuration_for(None, None)
        .unwrap();
    let selected = pascal_project::installations::select_installation(
        &config,
        &project,
        &InstallationEvidence {
            compiler_version: Some(pascal_project::CompilerVersion::new(24, 0)),
            ..InstallationEvidence::default()
        },
        None,
    );
    assert_eq!(
        selected,
        InstallationSelection::Selected {
            id: "10.0".into(),
            origin: InstallationOrigin::Metadata,
        }
    );
}

#[test]
fn project_bootstrap_reads_direct_identity_and_configuration_defaults() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("App.dproj");
    let source = temp.path().join("App.dpr");
    write(&source, "begin end.\n");
    write(
        &project,
        r#"<Project>
          <PropertyGroup>
            <ProjectVersion>20.3</ProjectVersion>
            <CompilerVersion>24.0</CompilerVersion>
            <Config>Release</Config>
            <Platform>Win32</Platform>
            <MainSource>App.dpr</MainSource>
          </PropertyGroup>
        </Project>"#,
    );
    let configuration = temp.path().join("config.toml");
    write(
        &configuration,
        "[installations.\"7.0\".properties]\nBDS = '/sdk/7'\n\
         [installations.\"10.0\".properties]\nBDS = '/sdk/10'\n",
    );
    let mut session_choices = std::collections::HashMap::new();
    session_choices.insert(project.clone(), "7.0".into());
    let context = ProjectContext::discover_with_overrides(
        &source,
        &[temp.path().to_path_buf()],
        &ProjectOptions {
            project_file: Some(project),
            installation_selections: session_choices,
            ..ProjectOptions::default()
        },
        &OverrideSession::new(Some(configuration)),
    )
    .unwrap();
    assert_eq!(context.config.as_deref(), Some("Release"));
    assert_eq!(context.platform.as_deref(), Some("Win32"));
    assert_eq!(
        context.conditional_context.compiler_version,
        Some(pascal_project::CompilerVersion::new(24, 0))
    );
    assert_eq!(
        context.installation_evidence.project_version.as_deref(),
        Some("20.3")
    );
    assert_eq!(
        context.installation_selection,
        Some(InstallationSelection::Selected {
            id: "7.0".into(),
            origin: InstallationOrigin::Session,
        })
    );
}

#[test]
fn bootstrap_conditions_do_not_accept_false_or_unknown_identity_groups() {
    let temp = tempfile::tempdir().unwrap();
    let configuration = temp.path().join("config.toml");
    write(
        &configuration,
        "[installations.\"10.0\".properties]\nBDS = '/sdk/10'\n",
    );
    let source = temp.path().join("App.dpr");
    write(&source, "begin end.\n");
    let project = temp.path().join("App.dproj");
    write(
        &project,
        r#"<Project><PropertyGroup Condition="'$(Platform)' == 'Win32'">
          <CompilerVersion>24.0</CompilerVersion><MainSource>App.dpr</MainSource>
        </PropertyGroup></Project>"#,
    );
    let false_context = ProjectContext::discover_with_overrides(
        &source,
        &[temp.path().to_path_buf()],
        &ProjectOptions {
            project_file: Some(project.clone()),
            platform: Some("Win64".into()),
            ..ProjectOptions::default()
        },
        &OverrideSession::new(Some(configuration.clone())),
    )
    .unwrap();
    assert_eq!(false_context.installation_evidence.compiler_version, None);
    assert_eq!(
        false_context.installation_selection,
        Some(InstallationSelection::NeedsChoice {
            candidates: vec!["10.0".into()]
        })
    );

    write(
        &project,
        r#"<Project><PropertyGroup>
          <CompilerVersion Condition="'$(Platform)' == 'Win32'">24.0</CompilerVersion>
          <MainSource>App.dpr</MainSource>
        </PropertyGroup></Project>"#,
    );
    let false_property_context = ProjectContext::discover_with_overrides(
        &source,
        &[temp.path().to_path_buf()],
        &ProjectOptions {
            project_file: Some(project.clone()),
            platform: Some("Win64".into()),
            ..ProjectOptions::default()
        },
        &OverrideSession::new(Some(configuration.clone())),
    )
    .unwrap();
    assert_eq!(
        false_property_context
            .installation_evidence
            .compiler_version,
        None
    );

    write(
        &project,
        r#"<Project><PropertyGroup>
          <CompilerVersion Condition="'$(UNKNOWN_PROPERTY)' == 'x'">24.0</CompilerVersion>
          <MainSource>App.dpr</MainSource>
        </PropertyGroup></Project>"#,
    );
    let unknown_context = ProjectContext::discover_with_overrides(
        &source,
        &[temp.path().to_path_buf()],
        &ProjectOptions {
            project_file: Some(project.clone()),
            ..ProjectOptions::default()
        },
        &OverrideSession::new(Some(configuration.clone())),
    )
    .unwrap();
    assert_eq!(unknown_context.installation_evidence.compiler_version, None);
    assert!(unknown_context.installation_evidence.conflicting);
    assert_eq!(
        unknown_context.installation_selection,
        Some(InstallationSelection::NeedsChoice {
            candidates: vec!["10.0".into()]
        })
    );

    write(
        &project,
        r#"<Project>
          <PropertyGroup><Config>Debug</Config></PropertyGroup>
          <PropertyGroup Condition="'$(Config)' == 'Debug'">
            <CompilerVersion>24.0</CompilerVersion><MainSource>App.dpr</MainSource>
          </PropertyGroup>
          <PropertyGroup Condition="'$(Config)' == 'Release'">
            <CompilerVersion>36.0</CompilerVersion>
          </PropertyGroup>
        </Project>"#,
    );
    let defaulted_context = ProjectContext::discover_with_overrides(
        &source,
        &[temp.path().to_path_buf()],
        &ProjectOptions {
            project_file: Some(project.clone()),
            ..ProjectOptions::default()
        },
        &OverrideSession::new(Some(configuration.clone())),
    )
    .unwrap();
    assert_eq!(defaulted_context.config.as_deref(), Some("Debug"));
    assert_eq!(
        defaulted_context.installation_evidence.compiler_version,
        Some(pascal_project::CompilerVersion::new(24, 0))
    );
}

#[test]
fn explicit_conditional_compiler_version_overrides_project_identity_for_selection() {
    let temp = tempfile::tempdir().unwrap();
    let configuration = temp.path().join("config.toml");
    write(
        &configuration,
        "[installations.\"10.0\".properties]\nBDS = '/sdk/10'\n\
         [installations.\"23.0\".properties]\nBDS = '/sdk/23'\n",
    );
    let project = temp.path().join("App.dproj");
    let source = temp.path().join("App.dpr");
    write(&source, "begin end.\n");
    write(
        &project,
        "<Project><PropertyGroup><CompilerVersion>24.0</CompilerVersion>\
         <MainSource>App.dpr</MainSource></PropertyGroup></Project>",
    );
    let explicit_version = pascal_project::CompilerVersion::new(36, 0);
    let context = ProjectContext::discover_with_overrides(
        &source,
        &[temp.path().to_path_buf()],
        &ProjectOptions {
            project_file: Some(project),
            conditional_context: pascal_project::ConditionalContext {
                compiler_version: Some(explicit_version),
                ..Default::default()
            },
            ..ProjectOptions::default()
        },
        &OverrideSession::new(Some(configuration)),
    )
    .unwrap();
    assert_eq!(
        context.conditional_context.compiler_version,
        Some(explicit_version)
    );
    assert_eq!(
        context.installation_selection,
        Some(InstallationSelection::Selected {
            id: "23.0".into(),
            origin: InstallationOrigin::Metadata,
        })
    );
}

#[test]
fn imported_compiler_metadata_does_not_supply_bootstrap_installation_identity() {
    let temp = tempfile::tempdir().unwrap();
    let configuration = temp.path().join("config.toml");
    let bds = temp.path().join("BDS10");
    fs::create_dir_all(&bds).unwrap();
    write(
        &configuration,
        &format!(
            "[installations.\"10.0\".properties]\nBDS = '{}'\n\
             [installations.\"23.0\".properties]\nBDS = '/sdk/23'\n",
            bds.display()
        ),
    );
    let project = temp.path().join("App.dproj");
    let source = temp.path().join("App.dpr");
    write(&source, "begin end.\n");
    write(
        &project,
        &format!(
            "<Project><PropertyGroup><BDS>{}</BDS><CompilerVersion>24.0</CompilerVersion>\
             <MainSource>App.dpr</MainSource></PropertyGroup>\
             <Import Project=\"$(BDS)/Version.props\"/></Project>",
            bds.display()
        ),
    );
    write(
        &bds.join("Version.props"),
        "<Project><PropertyGroup><CompilerVersion>36.0</CompilerVersion></PropertyGroup></Project>",
    );
    let context = ProjectContext::discover_with_overrides(
        &source,
        &[temp.path().to_path_buf()],
        &ProjectOptions {
            project_file: Some(project),
            ..ProjectOptions::default()
        },
        &OverrideSession::new(Some(configuration)),
    )
    .unwrap();
    assert!(context.metadata_files.contains(&bds.join("Version.props")));
    assert_eq!(
        context.installation_evidence.compiler_version,
        Some(pascal_project::CompilerVersion::new(24, 0))
    );
    assert_eq!(
        context.installation_evidence.bds_root.as_deref(),
        Some(bds.to_str().unwrap())
    );
    assert_eq!(
        context.installation_selection,
        Some(InstallationSelection::Selected {
            id: "10.0".into(),
            origin: InstallationOrigin::Metadata,
        })
    );
}

#[test]
fn import_may_set_config_so_empty_config_identity_condition_is_unknown() {
    let temp = tempfile::tempdir().unwrap();
    let configuration = temp.path().join("config.toml");
    write(
        &configuration,
        "[installations.\"10.0\".properties]\nBDS = '/sdk/10'\n\
         [installations.\"23.0\".properties]\nBDS = '/sdk/23'\n",
    );
    let source = temp.path().join("App.dpr");
    let project = temp.path().join("App.dproj");
    write(&source, "begin end.\n");
    write(
        &temp.path().join("Config.props"),
        "<Project><PropertyGroup><Config>Release</Config></PropertyGroup></Project>",
    );
    write(
        &project,
        r#"<Project>
          <Import Project="Config.props" />
          <PropertyGroup Condition="'$(Config)' == ''">
            <CompilerVersion>24.0</CompilerVersion>
          </PropertyGroup>
          <PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup>
        </Project>"#,
    );
    let context = ProjectContext::discover_with_overrides(
        &source,
        &[temp.path().to_path_buf()],
        &ProjectOptions {
            project_file: Some(project),
            ..ProjectOptions::default()
        },
        &OverrideSession::new(Some(configuration)),
    )
    .unwrap();
    assert_eq!(context.config.as_deref(), Some("Release"));
    assert_eq!(context.installation_evidence.compiler_version, None);
    assert!(context.installation_evidence.conflicting);
    assert_eq!(
        context.installation_selection,
        Some(InstallationSelection::NeedsChoice {
            candidates: vec!["10.0".into(), "23.0".into()]
        })
    );
}

#[test]
fn direct_compiler_fact_survives_unrelated_imported_config_uncertainty() {
    let temp = tempfile::tempdir().unwrap();
    let configuration = temp.path().join("config.toml");
    write(
        &configuration,
        "[installations.\"10.0\".properties]\nBDS = '/sdk/10'\n\
         [installations.\"23.0\".properties]\nBDS = '/sdk/23'\n",
    );
    let source = temp.path().join("App.dpr");
    let project = temp.path().join("App.dproj");
    write(&source, "begin end.\n");
    write(
        &temp.path().join("Config.props"),
        "<Project><PropertyGroup><Config>Release</Config></PropertyGroup></Project>",
    );
    write(
        &project,
        r#"<Project>
          <Import Project="Config.props" />
          <PropertyGroup><CompilerVersion>24.0</CompilerVersion><MainSource>App.dpr</MainSource></PropertyGroup>
          <PropertyGroup Condition="'$(Config)' == ''"><Platform>Win32</Platform></PropertyGroup>
        </Project>"#,
    );
    let context = ProjectContext::discover_with_overrides(
        &source,
        &[temp.path().to_path_buf()],
        &ProjectOptions {
            project_file: Some(project),
            ..ProjectOptions::default()
        },
        &OverrideSession::new(Some(configuration)),
    )
    .unwrap();
    assert_eq!(
        context.installation_evidence.compiler_version,
        Some(pascal_project::CompilerVersion::new(24, 0))
    );
    assert_eq!(
        context.installation_selection,
        Some(InstallationSelection::Selected {
            id: "10.0".into(),
            origin: InstallationOrigin::Metadata,
        })
    );
}

#[test]
fn explicit_compiler_resolves_compiler_conflicts_but_not_bds_conflicts() {
    let temp = tempfile::tempdir().unwrap();
    let configuration = temp.path().join("config.toml");
    write(
        &configuration,
        "[installations.\"10.0\".properties]\nBDS = '/sdk/10'\n\
         [installations.\"23.0\".properties]\nBDS = '/sdk/23'\n",
    );
    let source = temp.path().join("App.dpr");
    let project = temp.path().join("App.dproj");
    write(&source, "begin end.\n");
    write(
        &project,
        "<Project><PropertyGroup><CompilerVersion>24.0</CompilerVersion>\
         <DCC_CompilerVersion>36.0</DCC_CompilerVersion><MainSource>App.dpr</MainSource>\
         </PropertyGroup></Project>",
    );
    let explicit_version = pascal_project::CompilerVersion::new(36, 0);
    let options = ProjectOptions {
        project_file: Some(project.clone()),
        conditional_context: pascal_project::ConditionalContext {
            compiler_version: Some(explicit_version),
            ..Default::default()
        },
        ..ProjectOptions::default()
    };
    let context = ProjectContext::discover_with_overrides(
        &source,
        &[temp.path().to_path_buf()],
        &options,
        &OverrideSession::new(Some(configuration.clone())),
    )
    .unwrap();
    assert!(context.installation_evidence.conflicting);
    assert_eq!(
        context.installation_selection,
        Some(InstallationSelection::Selected {
            id: "23.0".into(),
            origin: InstallationOrigin::Metadata,
        })
    );

    write(
        &project,
        "<Project><PropertyGroup><CompilerVersion>24.0</CompilerVersion>\
         <DCC_CompilerVersion>36.0</DCC_CompilerVersion><BDS>/sdk/not-configured</BDS>\
         <MainSource>App.dpr</MainSource></PropertyGroup></Project>",
    );
    let independent_conflict = ProjectContext::discover_with_overrides(
        &source,
        &[temp.path().to_path_buf()],
        &options,
        &OverrideSession::new(Some(configuration)),
    )
    .unwrap();
    assert_eq!(
        independent_conflict.installation_selection,
        Some(InstallationSelection::NeedsChoice {
            candidates: vec!["10.0".into(), "23.0".into()]
        })
    );
}

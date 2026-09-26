use std::{fs, path::Path};

use pascal_project::delphi_overrides::OverrideSession;

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
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

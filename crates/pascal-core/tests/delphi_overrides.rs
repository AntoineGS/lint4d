use std::fs;
use std::path::Path;

#[cfg(unix)]
use std::io;
#[cfg(unix)]
use std::path::PathBuf;

#[cfg(unix)]
use std::process::{Child, Command, ExitStatus};
#[cfg(unix)]
use std::time::{Duration, Instant};

use pascal_core::delphi_overrides::{
    EffectiveOverrides, LOCAL_CONFIG_NAME, MAX_CONFIG_BYTES, OverrideLayer, OverrideSession,
    user_config_path,
};

#[test]
fn delphi_overrides_merge_properties_without_erasing_unrelated_keys() {
    let user = OverrideLayer::parse(
        "[properties]\nBDS = 'C:\\SDK'\nPlatform = 'Win32'\n",
        Path::new("/config/user.toml"),
    )
    .unwrap();
    let project = OverrideLayer::parse(
        "[properties]\nbds = 'D:\\OtherSDK'\n",
        Path::new("/project/.delphi-tools.local.toml"),
    )
    .unwrap();
    let effective = EffectiveOverrides::merge(&[user, project]);
    assert_eq!(effective.properties["bds"], r"D:\OtherSDK");
    assert_eq!(effective.properties["platform"], "Win32");
    assert_eq!(
        effective.property_origins["bds"],
        Path::new("/project/.delphi-tools.local.toml")
    );
}

#[test]
fn delphi_overrides_reject_invalid_property_inputs() {
    for text in [
        "[properties]\nBDS = 'a'\nbds = 'b'",
        "[properties]\nBDS = 42",
        "[properties]\nBDS = '$(OTHER)'",
        "[properties]\nThisFileDirectory = '/tmp'",
        "[properties]\nMSBuildThisFileDirectory = '/tmp'",
        "[properties]\n'bad-name' = 'x'",
        "unknown_option = true",
    ] {
        let error = OverrideLayer::parse(text, Path::new("/config/bad.toml")).unwrap_err();
        assert!(error.contains("/config/bad.toml"), "{error}");
    }
    let empty = OverrideLayer::parse("[properties]\nBDS = ''", Path::new("/c")).unwrap();
    assert_eq!(EffectiveOverrides::merge(&[empty]).properties["bds"], "");
}

#[test]
fn delphi_overrides_reject_invalid_mapping_records() {
    for text in [
        "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '/tmp'\nextra = true",
        "[[path_mappings]]\nfrom = 'C:\\SDK'",
        "[[path_mappings]]\nto = '/tmp'",
        "[[path_mappings]]\nfrom = 42\nto = '/tmp'",
    ] {
        let error = OverrideLayer::parse(text, Path::new("/config/bad.toml")).unwrap_err();
        assert!(error.contains("/config/bad.toml"), "{error}");
    }
}

#[cfg(unix)]
#[test]
fn delphi_overrides_map_longest_component_prefix() {
    let layer = OverrideLayer::parse(
        r#"
[[path_mappings]]
from = 'C:\SDK'
to = '/opt/sdk'
[[path_mappings]]
from = 'C:\SDK\lib\Indy10'
to = '/src/indy'
"#,
        Path::new("/config/overrides.toml"),
    )
    .unwrap();
    let settings = EffectiveOverrides::merge(&[layer]);
    let resolved = settings
        .resolve_path(r"c:/sdk/lib/INDY10/IdGlobal.pas", Path::new("/work"))
        .unwrap();
    assert_eq!(resolved.path, Path::new("/src/indy/IdGlobal.pas"));
    assert_eq!(
        resolved.mapping.unwrap().config_file,
        Path::new("/config/overrides.toml")
    );
    assert!(
        settings
            .resolve_path(r"C:\SDK-old\Bad.pas", Path::new("/work"))
            .is_err()
    );
    assert_eq!(settings.read_roots().len(), 2);
}

#[test]
fn delphi_overrides_merge_mappings_replaces_same_prefix_and_retains_order() {
    let root = tempfile::tempdir().unwrap();
    let user_other = root.path().join("user-other");
    let user_sdk = root.path().join("user-sdk");
    let project_sdk = root.path().join("project-sdk");
    let project_extra = root.path().join("project-extra");
    let user = OverrideLayer::parse(
        &format!(
            r#"
[[path_mappings]]
from = 'D:\Other\'
to = '{}'
[[path_mappings]]
from = 'C:\SDK'
to = '{}'
"#,
            user_other.display(),
            user_sdk.display(),
        ),
        Path::new("/config/user.toml"),
    )
    .unwrap();
    let project = OverrideLayer::parse(
        &format!(
            r#"
[[path_mappings]]
from = 'c:/sdk///'
to = '{}'
[[path_mappings]]
from = 'E:\Extra'
to = '{}'
"#,
            project_sdk.display(),
            project_extra.display(),
        ),
        Path::new("/project/.delphi-tools.local.toml"),
    )
    .unwrap();

    let effective = EffectiveOverrides::merge(&[user, project]);

    assert_eq!(
        effective
            .path_mappings
            .iter()
            .map(|mapping| mapping.from.as_str())
            .collect::<Vec<_>>(),
        vec!["c:/sdk", "d:/other", "e:/extra"]
    );
    assert_eq!(effective.path_mappings[0].to, project_sdk);
    assert_eq!(
        effective.path_mappings[0].config_file,
        Path::new("/project/.delphi-tools.local.toml")
    );
    assert_eq!(effective.path_mappings[1].to, user_other);
    assert_eq!(effective.path_mappings[2].to, project_extra);
}

#[test]
fn delphi_overrides_normalize_mapping_prefixes_and_reject_invalid_records() {
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("normalized");
    let layer = OverrideLayer::parse(
        &format!(
            r#"
[[path_mappings]]
from = 'C:\SDK\\.\lib\Indy10\\'
to = '{}'
"#,
            destination.display(),
        ),
        Path::new("/config/overrides.toml"),
    )
    .unwrap();
    let effective = EffectiveOverrides::merge(&[layer]);
    assert_eq!(effective.path_mappings[0].from, "c:/sdk/lib/indy10");
    assert_eq!(effective.path_mappings[0].to, destination);

    let duplicate = OverrideLayer::parse(
        &format!(
            r#"
[[path_mappings]]
from = 'C:\SDK'
to = '{}'
[[path_mappings]]
from = 'c:/sdk///'
to = '{}'
"#,
            root.path().join("one").display(),
            root.path().join("two").display(),
        ),
        Path::new("/config/duplicate.toml"),
    )
    .unwrap_err();
    assert!(
        duplicate.contains("duplicate normalized mapping prefix"),
        "{duplicate}"
    );

    let valid_destination = root.path().display().to_string();
    for (from, to, expected) in [
        ("", valid_destination.as_str(), "source prefix"),
        ("SDK", valid_destination.as_str(), "source prefix"),
        (r"\SDK", valid_destination.as_str(), "source prefix"),
        (r"C:SDK", valid_destination.as_str(), "drive-relative"),
        (r"\\?\C:\SDK", valid_destination.as_str(), "device"),
        (r"\\.\device", valid_destination.as_str(), "device"),
        (r"C:\SDK\..\Other", valid_destination.as_str(), ".."),
        (r"C:\SDK", "", "destination"),
        (r"C:\SDK", "relative/destination", "destination"),
        (r"C:\SDK", "/tmp/../other", ".."),
        (r"C:\SDK", "$(HOME)/sdk", "destination"),
        (r"C:\SDK", "~/sdk", "destination"),
    ] {
        let text = format!("[[path_mappings]]\nfrom = '{from}'\nto = '{to}'\n");
        let error = OverrideLayer::parse(&text, Path::new("/config/bad-mapping.toml")).unwrap_err();
        assert!(error.contains(expected), "{from:?} -> {to:?}: {error}");
        assert!(error.contains("/config/bad-mapping.toml"), "{error}");
    }
}

#[cfg(unix)]
#[test]
fn delphi_overrides_normalize_input_components_and_preserve_suffix_spelling() {
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("normalized");
    let layer = OverrideLayer::parse(
        &format!(
            r#"
[[path_mappings]]
from = 'C:\SDK\lib\\'
to = '{}'
"#,
            destination.display(),
        ),
        Path::new("/config/overrides.toml"),
    )
    .unwrap();
    let settings = EffectiveOverrides::merge(&[layer]);

    let resolved = settings
        .resolve_path(r"c:/sdk//LIB/./nested/../uNiT.Pas", Path::new("/work"))
        .unwrap();

    assert_eq!(resolved.path, destination.join("uNiT.Pas"));
    assert_eq!(resolved.mapping.unwrap().from, "c:/sdk/lib");
}

#[cfg(unix)]
#[test]
fn delphi_overrides_map_drive_and_unc_roots() {
    let root = tempfile::tempdir().unwrap();
    let drive_destination = root.path().join("drive");
    let unc_destination = root.path().join("unc");
    let layer = OverrideLayer::parse(
        &format!(
            r#"
[[path_mappings]]
from = 'C:\'
to = '{}'
[[path_mappings]]
from = '\\server\share'
to = '{}'
"#,
            drive_destination.display(),
            unc_destination.display(),
        ),
        Path::new("/config/roots.toml"),
    )
    .unwrap();
    let settings = EffectiveOverrides::merge(&[layer]);

    assert_eq!(
        settings
            .resolve_path(r"C:\Unit.pas", Path::new("/work"))
            .unwrap()
            .path,
        drive_destination.join("Unit.pas")
    );
    assert_eq!(
        settings
            .resolve_path(r"C:\", Path::new("/work"))
            .unwrap()
            .path,
        drive_destination
    );

    let resolved_unc = settings
        .resolve_path(r"\\SERVER\SHARE\Sub\Unit.pas", Path::new("/work"))
        .unwrap();
    assert_eq!(
        resolved_unc.path,
        unc_destination.join("Sub").join("Unit.pas")
    );
    assert_eq!(resolved_unc.mapping.unwrap().from, "//server/share");
    assert_eq!(
        settings
            .resolve_path(r"\\server\share", Path::new("/work"))
            .unwrap()
            .path,
        unc_destination
    );
    assert_eq!(
        settings.read_roots(),
        vec![unc_destination, drive_destination]
    );
}

#[cfg(unix)]
#[test]
fn delphi_overrides_reject_unsupported_windows_inputs_and_component_escape() {
    let root = tempfile::tempdir().unwrap();
    let layer = OverrideLayer::parse(
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            root.path().display()
        ),
        Path::new("/config/overrides.toml"),
    )
    .unwrap();
    let settings = EffectiveOverrides::merge(&[layer]);

    for (raw, expected) in [
        (r"C:foo", "drive-relative"),
        (r"\\?\C:\SDK\Unit.pas", "device"),
        (r"\\.\device\Unit.pas", "device"),
        (r"C:\..\x", "escapes"),
        (
            r"C:\SDK\..\outside.pas",
            "Windows path is unavailable on Linux",
        ),
    ] {
        let error = settings.resolve_path(raw, Path::new("/work")).unwrap_err();
        assert!(error.contains(expected), "{raw}: {error}");
    }
}

#[cfg(unix)]
#[test]
fn delphi_overrides_reject_native_prefix_in_mapped_suffix() {
    let root = tempfile::tempdir().unwrap();
    let layer = OverrideLayer::parse(
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            root.path().display()
        ),
        Path::new("/config/overrides.toml"),
    )
    .unwrap();
    let settings = EffectiveOverrides::merge(&[layer]);

    let error = settings
        .resolve_path(r"C:\SDK\D:\outside.pas", Path::new("/work"))
        .unwrap_err();
    assert!(error.contains("native path prefix or root"), "{error}");
}

#[test]
fn delphi_overrides_leave_native_absolute_paths_and_normalize_relative_paths() {
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("mapped");
    let layer = OverrideLayer::parse(
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\SDK'\nto = '{}'\n",
            destination.display()
        ),
        Path::new("/config/overrides.toml"),
    )
    .unwrap();
    let settings = EffectiveOverrides::merge(&[layer]);
    let base = root.path().join("project");
    let native_absolute = base.join("Native").join("Unit.pas");

    let native = settings
        .resolve_path(native_absolute.to_str().unwrap(), &base)
        .unwrap();
    assert_eq!(native.path, native_absolute);
    assert!(native.mapping.is_none());

    let relative = settings.resolve_path(r"src\Unit.pas", &base).unwrap();
    assert_eq!(relative.path, base.join("src").join("Unit.pas"));
    assert!(relative.mapping.is_none());
}

#[cfg(unix)]
#[test]
fn delphi_overrides_keep_inherited_specific_prefix_when_broad_prefix_is_replaced() {
    let root = tempfile::tempdir().unwrap();
    let broad_destination = root.path().join("broad");
    let specific_destination = root.path().join("missing-specific");

    let user = OverrideLayer::parse(
        &format!(
            r#"
[[path_mappings]]
from = 'C:\SDK'
to = '{}'
[[path_mappings]]
from = 'C:\SDK\lib'
to = '{}'
"#,
            broad_destination.display(),
            specific_destination.display(),
        ),
        Path::new("/config/user.toml"),
    )
    .unwrap();
    let project_destination = root.path().join("project-broad");
    std::fs::create_dir(&project_destination).unwrap();
    let project = OverrideLayer::parse(
        &format!(
            "[[path_mappings]]\nfrom = 'c:/sdk/'\nto = '{}'\n",
            project_destination.display()
        ),
        Path::new("/project/.delphi-tools.local.toml"),
    )
    .unwrap();
    let settings = EffectiveOverrides::merge(&[user, project]);

    assert!(!specific_destination.exists());
    assert!(project_destination.exists());
    let specific = settings
        .resolve_path(r"C:\SDK\lib\Unit.pas", Path::new("/work"))
        .unwrap();
    assert_eq!(specific.path, specific_destination.join("Unit.pas"));
    assert_eq!(
        specific.mapping.unwrap().config_file,
        Path::new("/config/user.toml")
    );

    let broad = settings
        .resolve_path(r"C:\SDK\bin\Compiler.exe", Path::new("/work"))
        .unwrap();
    assert_eq!(
        broad.path,
        project_destination.join("bin").join("Compiler.exe")
    );
    assert_eq!(
        broad.mapping.unwrap().config_file,
        Path::new("/project/.delphi-tools.local.toml")
    );
}

#[test]
fn delphi_overrides_capture_absence_and_share_it_with_workers() {
    let root = tempfile::tempdir().unwrap();
    let session = OverrideSession::new(None);
    session.capture_workspace(root.path()).unwrap();
    fs::write(
        root.path().join(LOCAL_CONFIG_NAME),
        "[properties]\nBDS = 'C:\\SDK'\n",
    )
    .unwrap();
    let worker = session.clone();
    assert!(
        worker
            .effective_for(Some(root.path()), None)
            .unwrap()
            .properties
            .is_empty()
    );
    let restarted = OverrideSession::new(None);
    assert_eq!(
        restarted
            .effective_for(Some(root.path()), None)
            .unwrap()
            .properties["bds"],
        r"C:\SDK"
    );
}

#[test]
fn delphi_overrides_user_config_path_prefers_absolute_xdg_and_falls_back_to_home() {
    let root = tempfile::tempdir().unwrap();
    let xdg = root.path().join("xdg");
    let home = root.path().join("home");

    assert_eq!(
        user_config_path(Some(&xdg), Some(&home)).unwrap(),
        xdg.join("delphi-tools").join("config.toml")
    );
    assert_eq!(
        user_config_path(Some(Path::new("relative-xdg")), Some(&home)).unwrap(),
        home.join(".config")
            .join("delphi-tools")
            .join("config.toml")
    );
    assert_eq!(
        user_config_path(Some(Path::new("")), Some(&home)).unwrap(),
        home.join(".config")
            .join("delphi-tools")
            .join("config.toml")
    );
}

#[test]
fn delphi_overrides_user_config_path_rejects_missing_or_unusable_home() {
    for (xdg, home) in [
        (None, None),
        (Some(Path::new("relative-xdg")), None),
        (Some(Path::new("")), Some(Path::new("relative-home"))),
        (None, Some(Path::new(""))),
    ] {
        let error = user_config_path(xdg, home).unwrap_err();
        assert!(error.contains("HOME"), "{error}");
    }
}

#[test]
fn delphi_overrides_effective_for_merges_user_workspace_and_project_layers() {
    let root = tempfile::tempdir().unwrap();
    let user_file = root.path().join("user.toml");
    let workspace = root.path().join("workspace");
    let project = workspace.join("project");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&project).unwrap();
    fs::write(&user_file, "[properties]\nBDS = 'user'\nUserOnly = 'yes'\n").unwrap();
    fs::write(
        workspace.join(LOCAL_CONFIG_NAME),
        "[properties]\nBDS = 'workspace'\nWorkspaceOnly = 'yes'\n",
    )
    .unwrap();
    fs::write(
        project.join(LOCAL_CONFIG_NAME),
        "[properties]\nBDS = 'project'\nProjectOnly = 'yes'\n",
    )
    .unwrap();

    let session = OverrideSession::new(Some(user_file.clone()));
    session.capture_workspace(&workspace).unwrap();
    let effective = session
        .effective_for(Some(&workspace), Some(&project))
        .unwrap();

    assert_eq!(effective.properties["bds"], "project");
    assert_eq!(effective.properties["useronly"], "yes");
    assert_eq!(effective.properties["workspaceonly"], "yes");
    assert_eq!(effective.properties["projectonly"], "yes");
    assert_eq!(
        effective.property_origins["bds"],
        project.join(LOCAL_CONFIG_NAME)
    );
}

#[test]
fn delphi_overrides_user_configuration_is_captured_when_the_session_starts() {
    let root = tempfile::tempdir().unwrap();
    let user_file = root.path().join("user.toml");
    fs::write(&user_file, "[properties]\nBDS = 'initial'\n").unwrap();
    let session = OverrideSession::new(Some(user_file.clone()));
    fs::write(&user_file, "[properties]\nBDS = 'replaced'\n").unwrap();

    let effective = session.effective_for(None, None).unwrap();
    assert_eq!(effective.properties["bds"], "initial");
}

#[test]
fn delphi_overrides_project_configuration_is_captured_on_first_effective_evaluation() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let project = workspace.join("project");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&project).unwrap();
    let session = OverrideSession::new(None);
    session.capture_workspace(&workspace).unwrap();
    fs::write(
        project.join(LOCAL_CONFIG_NAME),
        "[properties]\nLazy = 'captured'\n",
    )
    .unwrap();

    let first = session
        .effective_for(Some(&workspace), Some(&project))
        .unwrap();
    fs::write(
        project.join(LOCAL_CONFIG_NAME),
        "[properties]\nLazy = 'changed'\n",
    )
    .unwrap();
    let second = session
        .effective_for(Some(&workspace), Some(&project))
        .unwrap();

    assert_eq!(first.properties["lazy"], "captured");
    assert_eq!(second.properties["lazy"], "captured");
}

#[test]
fn delphi_overrides_same_directory_workspace_and_project_are_captured_once() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join(LOCAL_CONFIG_NAME),
        "[properties]\nShared = 'before'\n",
    )
    .unwrap();
    let session = OverrideSession::new(None);
    session.capture_workspace(root.path()).unwrap();
    fs::remove_file(root.path().join(LOCAL_CONFIG_NAME)).unwrap();
    fs::write(
        root.path().join(LOCAL_CONFIG_NAME),
        "[properties]\nShared = 'after'\n",
    )
    .unwrap();

    let effective = session
        .effective_for(Some(root.path()), Some(root.path()))
        .unwrap();
    assert_eq!(effective.properties["shared"], "before");
}

#[test]
fn delphi_overrides_cache_keys_are_absolute_lexical_native_paths() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let lexical_alias = root
        .path()
        .join("workspace")
        .join(".")
        .join("nested")
        .join("..");
    let session = OverrideSession::new(None);
    session.capture_workspace(&lexical_alias).unwrap();
    fs::write(
        workspace.join(LOCAL_CONFIG_NAME),
        "[properties]\nCached = 'absence'\n",
    )
    .unwrap();

    let effective = session.effective_for(Some(&workspace), None).unwrap();
    assert!(effective.properties.is_empty());
}

#[test]
fn delphi_overrides_malformed_lower_layers_are_not_hidden_by_higher_layers() {
    let root = tempfile::tempdir().unwrap();
    let user_file = root.path().join("user.toml");
    let workspace = root.path().join("workspace");
    let project = workspace.join("project");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&project).unwrap();
    fs::write(&user_file, "[properties\ninvalid = 'user'\n").unwrap();
    fs::write(
        workspace.join(LOCAL_CONFIG_NAME),
        "[properties]\nValue = 'workspace'\n",
    )
    .unwrap();
    fs::write(
        project.join(LOCAL_CONFIG_NAME),
        "[properties]\nValue = 'project'\n",
    )
    .unwrap();

    let session = OverrideSession::new(Some(user_file.clone()));
    session.capture_workspace(&workspace).unwrap();
    let error = session
        .effective_for(Some(&workspace), Some(&project))
        .unwrap_err();
    assert!(error.contains(&user_file.display().to_string()), "{error}");
}

#[test]
fn delphi_overrides_workspace_errors_are_not_hidden_by_valid_project_layers() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let project = workspace.join("project");
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(&project).unwrap();
    let workspace_file = workspace.join(LOCAL_CONFIG_NAME);
    fs::write(&workspace_file, "[properties\ninvalid = 'workspace'\n").unwrap();
    fs::write(
        project.join(LOCAL_CONFIG_NAME),
        "[properties]\nValue = 'project'\n",
    )
    .unwrap();

    let session = OverrideSession::new(None);
    let _ = session.capture_workspace(&workspace);
    let error = session
        .effective_for(Some(&workspace), Some(&project))
        .unwrap_err();
    assert!(
        error.contains(&workspace_file.display().to_string()),
        "{error}"
    );
}

#[test]
fn delphi_overrides_workspace_capture_reports_and_retains_read_errors() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join(LOCAL_CONFIG_NAME);
    fs::write(&config, "[properties\ninvalid = 'workspace'\n").unwrap();
    let session = OverrideSession::new(None);

    let capture_error = session.capture_workspace(root.path()).unwrap_err();
    assert!(
        capture_error.contains(&config.display().to_string()),
        "{capture_error}"
    );
    let worker = session.clone();
    fs::remove_file(&config).unwrap();
    fs::write(&config, "[properties]\nValue = 'replacement'\n").unwrap();
    let repeated_error = worker.capture_workspace(root.path()).unwrap_err();
    assert!(
        repeated_error.contains(&config.display().to_string()),
        "{repeated_error}"
    );
    assert_eq!(repeated_error, capture_error);
    let effective_error = worker.effective_for(Some(root.path()), None).unwrap_err();
    assert!(
        effective_error.contains(&config.display().to_string()),
        "{effective_error}"
    );
    assert_eq!(effective_error, capture_error);

    let restarted = OverrideSession::new(None);
    let replacement = restarted.effective_for(Some(root.path()), None).unwrap();
    assert_eq!(replacement.properties["value"], "replacement");
}

#[test]
fn delphi_overrides_removal_and_readdition_do_not_refresh_a_captured_file() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join(LOCAL_CONFIG_NAME);
    fs::write(&config, "[properties]\nValue = 'first'\n").unwrap();
    let session = OverrideSession::new(None);
    session.capture_workspace(root.path()).unwrap();

    fs::remove_file(&config).unwrap();
    session.capture_workspace(root.path()).unwrap();
    fs::write(&config, "[properties]\nValue = 'second'\n").unwrap();
    session.capture_workspace(root.path()).unwrap();

    let effective = session.effective_for(Some(root.path()), None).unwrap();
    assert_eq!(effective.properties["value"], "first");
}

#[test]
fn delphi_overrides_accepts_exactly_four_mib_and_rejects_one_byte_more() {
    let root = tempfile::tempdir().unwrap();
    let exact_root = root.path().join("exact");
    let oversized_root = root.path().join("oversized");
    fs::create_dir(&exact_root).unwrap();
    fs::create_dir(&oversized_root).unwrap();

    let prefix = b"[properties]\nBDS = 'x'\n";
    let mut exact = vec![b' '; MAX_CONFIG_BYTES];
    exact[..prefix.len()].copy_from_slice(prefix);
    fs::write(exact_root.join(LOCAL_CONFIG_NAME), &exact).unwrap();
    let exact_session = OverrideSession::new(None);
    exact_session.capture_workspace(&exact_root).unwrap();
    assert_eq!(
        exact_session
            .effective_for(Some(&exact_root), None)
            .unwrap()
            .properties["bds"],
        "x"
    );

    let mut oversized = exact;
    oversized.push(b' ');
    let oversized_file = oversized_root.join(LOCAL_CONFIG_NAME);
    fs::write(&oversized_file, oversized).unwrap();
    let oversized_session = OverrideSession::new(None);
    let _ = oversized_session.capture_workspace(&oversized_root);
    let error = oversized_session
        .effective_for(Some(&oversized_root), None)
        .unwrap_err();
    assert!(
        error.contains(&oversized_file.display().to_string()),
        "{error}"
    );
    assert!(error.contains("exceeds"), "{error}");
}

#[test]
fn delphi_overrides_rejects_directories_and_invalid_utf8_with_provenance() {
    let root = tempfile::tempdir().unwrap();
    let directory_root = root.path().join("directory");
    let invalid_root = root.path().join("invalid");
    fs::create_dir(&directory_root).unwrap();
    fs::create_dir(&invalid_root).unwrap();
    let directory_file = directory_root.join(LOCAL_CONFIG_NAME);
    let invalid_file = invalid_root.join(LOCAL_CONFIG_NAME);
    fs::create_dir(&directory_file).unwrap();
    fs::write(&invalid_file, [0xff, 0xfe]).unwrap();

    let directory_session = OverrideSession::new(None);
    let _ = directory_session.capture_workspace(&directory_root);
    let directory_error = directory_session
        .effective_for(Some(&directory_root), None)
        .unwrap_err();
    assert!(
        directory_error.contains(&directory_file.display().to_string()),
        "{directory_error}"
    );
    assert!(directory_error.contains("directory"), "{directory_error}");

    let invalid_session = OverrideSession::new(None);
    let _ = invalid_session.capture_workspace(&invalid_root);
    let invalid_error = invalid_session
        .effective_for(Some(&invalid_root), None)
        .unwrap_err();
    assert!(
        invalid_error.contains(&invalid_file.display().to_string()),
        "{invalid_error}"
    );
    assert!(invalid_error.contains("invalid UTF-8"), "{invalid_error}");
}

#[cfg(unix)]
#[test]
fn delphi_overrides_allows_a_symlink_to_a_regular_configuration_file() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("real.toml");
    let config = root.path().join(LOCAL_CONFIG_NAME);
    fs::write(&target, "[properties]\nViaLink = 'yes'\n").unwrap();
    symlink(&target, &config).unwrap();

    let session = OverrideSession::new(None);
    session.capture_workspace(root.path()).unwrap();
    let effective = session.effective_for(Some(root.path()), None).unwrap();
    assert_eq!(effective.properties["vialink"], "yes");
}

#[cfg(unix)]
struct FifoChildGuard {
    children: Vec<Option<Child>>,
}

#[cfg(unix)]
impl FifoChildGuard {
    fn new() -> Self {
        Self {
            children: Vec::new(),
        }
    }

    fn spawn(&mut self, command: &mut Command) -> io::Result<usize> {
        let child = command.spawn()?;
        self.children.push(Some(child));
        Ok(self.children.len() - 1)
    }

    fn try_wait(&mut self, index: usize) -> io::Result<Option<ExitStatus>> {
        self.children[index]
            .as_mut()
            .map_or(Ok(None), |child| child.try_wait())
    }

    fn cleanup(&mut self) {
        for child in self.children.iter_mut().flatten() {
            match child.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    let _ = child.kill();
                }
            }
        }
        for child in &mut self.children {
            if let Some(mut child) = child.take() {
                let _ = child.wait();
            }
        }
    }

    fn is_reaped(&self, index: usize) -> bool {
        self.children[index].is_none()
    }
}

#[cfg(unix)]
impl Drop for FifoChildGuard {
    fn drop(&mut self) {
        self.cleanup();
    }
}

#[cfg(unix)]
#[test]
fn delphi_overrides_fifo_child_guard_reaps_spawned_children() {
    let mut guard = FifoChildGuard::new();
    let mut command = Command::new("sleep");
    command.arg("30");
    let child = guard.spawn(&mut command).unwrap();

    guard.cleanup();

    assert!(guard.is_reaped(child));
}

#[cfg(unix)]
#[test]
fn delphi_overrides_fifo_candidates_are_rejected_before_opening() {
    if let Some(writer_path) = std::env::var_os("LINT4D_DELPHI_OVERRIDES_FIFO_WRITER_PATH") {
        let ready_path =
            PathBuf::from(std::env::var_os("LINT4D_DELPHI_OVERRIDES_FIFO_WRITER_READY").unwrap());
        fs::write(ready_path, []).unwrap();
        let _file = fs::OpenOptions::new()
            .write(true)
            .open(writer_path)
            .unwrap();
        return;
    }

    if std::env::var_os("LINT4D_DELPHI_OVERRIDES_FIFO_LOADER_CHILD").is_some() {
        let direct_root =
            PathBuf::from(std::env::var_os("LINT4D_DELPHI_OVERRIDES_FIFO_DIRECT_ROOT").unwrap());
        let symlink_root =
            PathBuf::from(std::env::var_os("LINT4D_DELPHI_OVERRIDES_FIFO_SYMLINK_ROOT").unwrap());
        let session = OverrideSession::new(None);
        let direct_error = session.capture_workspace(&direct_root).unwrap_err();
        assert!(
            direct_error.contains("not a regular file"),
            "{direct_error}"
        );
        let symlink_error = session.capture_workspace(&symlink_root).unwrap_err();
        assert!(
            symlink_error.contains("not a regular file"),
            "{symlink_error}"
        );
        return;
    }

    let root = tempfile::tempdir().unwrap();
    let direct_root = root.path().join("direct");
    let symlink_root = root.path().join("symlink");
    fs::create_dir(&direct_root).unwrap();
    fs::create_dir(&symlink_root).unwrap();
    let direct_fifo = direct_root.join(LOCAL_CONFIG_NAME);
    let symlink_target = root.path().join("symlink-target");
    let symlink_config = symlink_root.join(LOCAL_CONFIG_NAME);
    for fifo in [&direct_fifo, &symlink_target] {
        let status = Command::new("mkfifo").arg(fifo).status().unwrap();
        assert!(status.success(), "mkfifo failed for {}", fifo.display());
    }
    std::os::unix::fs::symlink(&symlink_target, &symlink_config).unwrap();

    let direct_ready = root.path().join("direct-ready");
    let symlink_ready = root.path().join("symlink-ready");
    let mut children = FifoChildGuard::new();
    let mut direct_writer_command = Command::new(std::env::current_exe().unwrap());
    direct_writer_command
        .args([
            "--exact",
            "delphi_overrides_fifo_candidates_are_rejected_before_opening",
            "--nocapture",
        ])
        .env("LINT4D_DELPHI_OVERRIDES_FIFO_WRITER_PATH", &direct_fifo)
        .env("LINT4D_DELPHI_OVERRIDES_FIFO_WRITER_READY", &direct_ready);
    let direct_writer = children.spawn(&mut direct_writer_command).unwrap();
    let mut symlink_writer_command = Command::new(std::env::current_exe().unwrap());
    symlink_writer_command
        .args([
            "--exact",
            "delphi_overrides_fifo_candidates_are_rejected_before_opening",
            "--nocapture",
        ])
        .env("LINT4D_DELPHI_OVERRIDES_FIFO_WRITER_PATH", &symlink_config)
        .env("LINT4D_DELPHI_OVERRIDES_FIFO_WRITER_READY", &symlink_ready);
    let symlink_writer = children.spawn(&mut symlink_writer_command).unwrap();
    let ready_deadline = Instant::now() + Duration::from_secs(1);
    while (!direct_ready.exists() || !symlink_ready.exists()) && Instant::now() < ready_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(direct_ready.exists(), "direct FIFO writer did not start");
    assert!(symlink_ready.exists(), "symlink FIFO writer did not start");
    std::thread::sleep(Duration::from_millis(100));

    let mut loader_command = Command::new(std::env::current_exe().unwrap());
    loader_command
        .args([
            "--exact",
            "delphi_overrides_fifo_candidates_are_rejected_before_opening",
            "--nocapture",
        ])
        .env("LINT4D_DELPHI_OVERRIDES_FIFO_LOADER_CHILD", "1")
        .env("LINT4D_DELPHI_OVERRIDES_FIFO_DIRECT_ROOT", &direct_root)
        .env("LINT4D_DELPHI_OVERRIDES_FIFO_SYMLINK_ROOT", &symlink_root);
    let loader = children.spawn(&mut loader_command).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = children.try_wait(loader).unwrap() {
            assert!(status.success(), "FIFO loader exited with {status}");
            break;
        }
        if Instant::now() >= deadline {
            children.cleanup();
            panic!("FIFO configuration inspection blocked");
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let direct_opened = children.try_wait(direct_writer).unwrap().is_some();
    let symlink_opened = children.try_wait(symlink_writer).unwrap().is_some();
    children.cleanup();
    assert!(!direct_opened, "direct FIFO was opened before validation");
    assert!(!symlink_opened, "symlink FIFO was opened before validation");
}

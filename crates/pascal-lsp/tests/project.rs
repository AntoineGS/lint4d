use lsp_types::{Position, Url};
use pascal_core::delphi_overrides::OverrideSession;
use pascal_lsp::project::{ProjectContext, ProjectOptions};
use pascal_lsp::workspace::{FileChange, Workspace, WorkspaceOptions};
#[cfg(target_os = "linux")]
use std::ffi::CString;
use std::fs;
#[cfg(target_os = "linux")]
use std::io::{self, Read};
#[cfg(target_os = "linux")]
use std::os::fd::FromRawFd;
#[cfg(target_os = "linux")]
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn inotify_init1(flags: i32) -> i32;
    fn inotify_add_watch(fd: i32, pathname: *const std::os::raw::c_char, mask: u32) -> i32;
}

#[cfg(target_os = "linux")]
const IN_NONBLOCK: i32 = 0x800;
#[cfg(target_os = "linux")]
const IN_OPEN: u32 = 0x0000_0020;

#[cfg(target_os = "linux")]
fn observed_open(path: &Path, operation: impl FnOnce()) -> bool {
    let fd = unsafe { inotify_init1(IN_NONBLOCK) };
    assert!(fd >= 0, "inotify_init1 failed");
    let pathname = CString::new(path.to_string_lossy().as_bytes()).expect("valid path");
    let watch = unsafe { inotify_add_watch(fd, pathname.as_ptr(), IN_OPEN) };
    assert!(watch >= 0, "inotify_add_watch failed");
    operation();

    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut events = [0_u8; 4096];
    match file.read(&mut events) {
        Ok(bytes) => {
            let mut offset = 0;
            while offset + 16 <= bytes {
                let mask = u32::from_ne_bytes(
                    events[offset + 4..offset + 8]
                        .try_into()
                        .expect("inotify mask bytes"),
                );
                let name_length = u32::from_ne_bytes(
                    events[offset + 12..offset + 16]
                        .try_into()
                        .expect("inotify name length bytes"),
                ) as usize;
                offset = offset.saturating_add(16).saturating_add(name_length);
                if mask & IN_OPEN != 0 {
                    return true;
                }
            }
            false
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => false,
        Err(error) => panic!("read inotify events: {error}"),
    }
}

#[cfg(target_os = "linux")]
#[test]
fn observed_open_reports_an_actual_positive_payload_read() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let path = temp.path().join("payload.txt");
    write(&path, "payload");

    let opened = observed_open(&path, || {
        let mut file = fs::File::open(&path).expect("payload file");
        let mut byte = [0_u8; 1];
        std::io::Read::read_exact(&mut file, &mut byte).expect("payload read");
        assert_eq!(byte, [b'p']);
    });

    assert!(opened, "the IN_OPEN observer must see a real positive read");
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create fixture directory");
    }
    fs::write(path, contents).expect("write fixture file");
}

fn options() -> ProjectOptions {
    ProjectOptions::default()
}

#[cfg(unix)]
fn mapped_session(root: &Path, properties: &str, from: &str, to: &Path) -> OverrideSession {
    write(
        &root.join(".delphi-tools.local.toml"),
        &format!(
            "[properties]\n{properties}\n[[path_mappings]]\nfrom = '{from}'\nto = '{}'\n",
            to.display(),
        ),
    );
    OverrideSession::new(None)
}

#[cfg(unix)]
#[test]
fn delphi_overrides_resolve_hardcoded_and_bds_search_paths() {
    let root = tempfile::tempdir().unwrap();
    let sdk = tempfile::tempdir().unwrap();
    write(
        &sdk.path().join("source/Provider.pas"),
        "unit Provider; interface implementation end.",
    );
    let main = root.path().join("Main.pas");
    write(
        &main,
        "unit Main; interface uses Provider; implementation end.",
    );
    write(
        &root.path().join("Main.dproj"),
        r#"
<Project><PropertyGroup>
  <DCC_UnitSearchPath>C:\SDK\source;$(BDS)\source</DCC_UnitSearchPath>
</PropertyGroup><ItemGroup><DCCReference Include="Main.pas"/></ItemGroup></Project>
"#,
    );
    write(
        &root.path().join(".delphi-tools.local.toml"),
        &format!(
            r#"[properties]
BDS = 'C:\SDK'
[[path_mappings]]
from = 'C:\SDK'
to = '{}'
"#,
            sdk.path().display(),
        ),
    );
    let context = ProjectContext::discover_with_overrides(
        &main,
        &[root.path().to_path_buf()],
        &options(),
        &OverrideSession::new(None),
    )
    .unwrap();
    assert!(context.search_paths.contains(&sdk.path().join("source")));
    assert!(!context.warnings.iter().any(|warning| {
        warning.contains("Windows path") || warning.contains("unresolved property")
    }));
}

#[cfg(unix)]
#[test]
fn unknown_bds_target_import_is_ignored_without_tainting_context() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let main = root.join("App.dpr");
    let units = root.join("units");
    fs::create_dir(&units).unwrap();
    write(&main, "program App; begin end.");
    write(
        &root.join("App.dproj"),
        r#"<Project>
<PropertyGroup><MainSource>App.dpr</MainSource><DCC_Define>KNOWN</DCC_Define><DCC_UnitSearchPath>units</DCC_UnitSearchPath></PropertyGroup>
<Import Project="$(BDS)\Bin\CodeGear.Delphi.Targets" />
<PropertyGroup><DCC_Define>AFTER;$(DCC_Define)</DCC_Define></PropertyGroup>
</Project>"#,
    );

    let context = discover(&main, root, &options());

    assert!(
        context.discovery_complete,
        "context was tainted: {context:?}"
    );
    assert_eq!(context.defines, ["AFTER", "KNOWN"]);
    assert_eq!(context.search_paths, vec![root.to_path_buf(), units]);
    assert!(context.warnings.iter().any(|warning| {
        warning.contains("ignored non-optset project import")
            && warning.contains("CodeGear.Delphi.Targets")
    }));
    assert!(
        !context
            .warnings
            .iter()
            .any(|warning| warning.contains("unresolved property")
                || warning.contains("Windows path"))
    );
}

#[cfg(unix)]
#[test]
fn known_unmapped_bds_target_import_is_ignored_without_tainting_context() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let main = root.join("App.dpr");
    let units = root.join("units");
    fs::create_dir(&units).unwrap();
    write(&main, "program App; begin end.");
    write(
        &root.join("App.dproj"),
        r#"<Project>
<PropertyGroup><MainSource>App.dpr</MainSource><BDS>C:\Delphi</BDS><DCC_Define>KNOWN</DCC_Define><DCC_UnitSearchPath>units</DCC_UnitSearchPath></PropertyGroup>
<Import Project="$(BDS)\Bin\CodeGear.Delphi.Targets" />
<PropertyGroup><DCC_Define>AFTER;$(DCC_Define)</DCC_Define></PropertyGroup>
</Project>"#,
    );

    let context = discover(&main, root, &options());

    assert!(
        context.discovery_complete,
        "context was tainted: {context:?}"
    );
    assert_eq!(context.defines, ["AFTER", "KNOWN"]);
    assert_eq!(context.search_paths, vec![root.to_path_buf(), units]);
    assert!(context.warnings.iter().any(|warning| {
        warning.contains("ignored non-optset project import")
            && warning.contains("CodeGear.Delphi.Targets")
    }));
    assert!(
        !context
            .warnings
            .iter()
            .any(|warning| warning.contains("unresolved property")
                || warning.contains("Windows path"))
    );
}

#[cfg(unix)]
#[test]
fn macro_before_literal_non_optset_suffix_is_ignored_without_tainting_context() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let main = root.join("App.dpr");
    let units = root.join("units");
    fs::create_dir(&units).unwrap();
    write(&main, "program App; begin end.");
    write(
        &root.join("App.dproj"),
        r#"<Project>
<PropertyGroup><MainSource>App.dpr</MainSource><DCC_Define>KNOWN</DCC_Define><DCC_UnitSearchPath>units</DCC_UnitSearchPath></PropertyGroup>
<Import Project="$(BDS)\Bin\CodeGear.$(Personality).Targets" />
<Import Project="$(TargetName).targets" />
<PropertyGroup><DCC_Define>AFTER;$(DCC_Define)</DCC_Define></PropertyGroup>
</Project>"#,
    );

    let context = discover(&main, root, &options());

    assert!(
        context.discovery_complete,
        "context was tainted: {context:?}"
    );
    assert_eq!(context.defines, ["AFTER", "KNOWN"]);
    assert_eq!(context.search_paths, vec![root.to_path_buf(), units]);
    for import in [
        "$(BDS)\\Bin\\CodeGear.$(Personality).Targets",
        "$(TargetName).targets",
    ] {
        assert!(context.warnings.iter().any(|warning| {
            warning.contains("ignored non-optset project import") && warning.contains(import)
        }));
    }
    assert!(
        !context
            .warnings
            .iter()
            .any(|warning| warning.contains("unresolved property")
                || warning.contains("Windows path"))
    );
}

#[cfg(unix)]
#[test]
fn native_xml_bds_paths_normalize_delphi_separators_for_exists_and_references() {
    let root_temp = tempfile::tempdir().unwrap();
    let sdk_temp = tempfile::tempdir().unwrap();
    let root = root_temp.path();
    let sdk = sdk_temp.path();
    let main = root.join("App.dpr");
    let provider = sdk.join("source/Provider.pas");
    write(&main, "program App; begin end.");
    write(&provider, "unit Provider; interface implementation end.");
    write(&sdk.join("source/enabled.flag"), "enabled");
    write(
        &root.join("App.dproj"),
        &format!(
            r#"<Project>
<PropertyGroup><MainSource>App.dpr</MainSource><BDS>{}</BDS><DCC_UnitSearchPath>$(BDS)\source</DCC_UnitSearchPath></PropertyGroup>
<ItemGroup><DCCReference Include="$(BDS)\source\Provider.pas" /></ItemGroup>
<PropertyGroup Condition="Exists('$(BDS)\source\enabled.flag')"><DCC_Define>NATIVE_EXISTS</DCC_Define></PropertyGroup>
</Project>"#,
            sdk.display(),
        ),
    );

    let context = discover(&main, root, &options());

    assert!(context.search_paths.contains(&sdk.join("source")));
    assert_eq!(context.defines, ["NATIVE_EXISTS"]);
    assert_eq!(
        context.explicit_units.get("provider"),
        Some(&vec![provider])
    );
    assert!(
        !context
            .warnings
            .iter()
            .any(|warning| warning.contains("path does not exist")
                || warning.contains("Windows path"))
    );
}

#[cfg(unix)]
#[test]
fn native_override_bds_paths_normalize_delphi_separators_for_exists_and_references() {
    let root_temp = tempfile::tempdir().unwrap();
    let sdk_temp = tempfile::tempdir().unwrap();
    let root = root_temp.path();
    let sdk = sdk_temp.path();
    let main = root.join("App.dpr");
    let provider = sdk.join("source/Provider.pas");
    write(&main, "program App; begin end.");
    write(&provider, "unit Provider; interface implementation end.");
    write(&sdk.join("source/enabled.flag"), "enabled");
    write(
        &root.join(".delphi-tools.local.toml"),
        &format!("[properties]\nBDS = '{}'\n", sdk.display()),
    );
    write(
        &root.join("App.dproj"),
        r#"<Project>
<PropertyGroup><MainSource>App.dpr</MainSource><BDS>wrong</BDS><DCC_UnitSearchPath>$(BDS)\source</DCC_UnitSearchPath></PropertyGroup>
<ItemGroup><DCCReference Include="$(BDS)\source\Provider.pas" /></ItemGroup>
<PropertyGroup Condition="Exists('$(BDS)\source\enabled.flag')"><DCC_Define>NATIVE_OVERRIDE_EXISTS</DCC_Define></PropertyGroup>
</Project>"#,
    );

    let context = discover(&main, root, &options());

    assert!(context.search_paths.contains(&sdk.join("source")));
    assert_eq!(context.defines, ["NATIVE_OVERRIDE_EXISTS"]);
    assert_eq!(
        context.explicit_units.get("provider"),
        Some(&vec![provider])
    );
    assert!(
        !context
            .warnings
            .iter()
            .any(|warning| warning.contains("path does not exist")
                || warning.contains("Windows path"))
    );
}

fn discover(file: &Path, root: &Path, options: &ProjectOptions) -> ProjectContext {
    let session = OverrideSession::new(None);
    ProjectContext::discover_with_overrides(file, &[root.to_path_buf()], options, &session)
        .expect("discover project")
}

#[test]
fn delphi_overrides_are_immutable_project_properties() {
    let root = tempfile::tempdir().unwrap();
    let main = root.path().join("Main.pas");
    write(&main, "unit Main; interface implementation end.");
    write(
        &root.path().join("Main.dproj"),
        r#"
<Project><PropertyGroup>
  <BDS>wrong</BDS><Config>Release</Config><Platform>Win64</Platform>
  <DCC_Namespace>$(BDS)</DCC_Namespace>
</PropertyGroup><ItemGroup><DCCReference Include="Main.pas"/></ItemGroup></Project>
"#,
    );
    write(
        &root.path().join(".delphi-tools.local.toml"),
        "[properties]\nBDS = 'Expected'\nConfig = 'Debug'\nPlatform = 'Win32'\n",
    );
    let session = OverrideSession::new(None);
    let context = ProjectContext::discover_with_overrides(
        &main,
        &[root.path().to_path_buf()],
        &options(),
        &session,
    )
    .unwrap();
    assert_eq!(context.config.as_deref(), Some("Debug"));
    assert_eq!(context.platform.as_deref(), Some("Win32"));
    assert!(
        context
            .unit_namespaces
            .iter()
            .any(|name| name == "Expected")
    );
}

#[test]
fn delphi_overrides_survive_optset_assignment_and_unknown_import_taint() {
    let root = tempfile::tempdir().unwrap();
    let main = root.path().join("App.dpr");
    write(&main, "program App; begin end.");
    write(
        &root.path().join("App.dproj"),
        r#"
<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup>
<Import Project="settings.optset" />
<Import Project="missing.optset" />
<PropertyGroup Condition="'$(MainSource)'=='App.dpr'"><DCC_Define>MAIN_SOURCE_KNOWN</DCC_Define></PropertyGroup>
<PropertyGroup><DCC_Namespace>$(BDS)</DCC_Namespace></PropertyGroup></Project>
"#,
    );
    write(
        &root.path().join("settings.optset"),
        r#"<Project><PropertyGroup><BDS>OptsetWrong</BDS></PropertyGroup></Project>"#,
    );
    write(
        &root.path().join(".delphi-tools.local.toml"),
        "[properties]\nBDS = 'Expected'\nMainSource = 'App.dpr'\n",
    );
    let session = OverrideSession::new(None);
    let context = ProjectContext::discover_with_overrides(
        &main,
        &[root.path().to_path_buf()],
        &options(),
        &session,
    )
    .unwrap();
    assert!(
        context
            .unit_namespaces
            .iter()
            .any(|name| name == "Expected")
    );
    assert!(
        !context
            .unit_namespaces
            .iter()
            .any(|name| name == "OptsetWrong")
    );
    assert_eq!(context.defines, ["MAIN_SOURCE_KNOWN"]);
}

#[test]
fn explicit_client_config_and_platform_override_file_properties() {
    let root = tempfile::tempdir().unwrap();
    let main = root.path().join("App.dpr");
    write(&main, "program App; begin end.");
    write(
        &root.path().join("App.dproj"),
        r#"<Project><PropertyGroup>
<MainSource>App.dpr</MainSource><Config>Project</Config><Platform>Project</Platform>
</PropertyGroup></Project>"#,
    );
    write(
        &root.path().join(".delphi-tools.local.toml"),
        "[properties]\nConfig = 'File'\nPlatform = 'File'\n",
    );
    let session = OverrideSession::new(None);
    let context = ProjectContext::discover_with_overrides(
        &main,
        &[root.path().to_path_buf()],
        &ProjectOptions {
            build_config: Some("Client".to_string()),
            platform: Some("Client".to_string()),
            ..ProjectOptions::default()
        },
        &session,
    )
    .unwrap();
    assert_eq!(context.config.as_deref(), Some("Client"));
    assert_eq!(context.platform.as_deref(), Some("Client"));
}

#[test]
fn immutable_globals_skip_unresolved_xml_rhs_expansion() {
    let client_root = tempfile::tempdir().unwrap();
    let client_main = client_root.path().join("App.dpr");
    write(&client_main, "program App; begin end.");
    write(
        &client_root.path().join("App.dproj"),
        r#"<Project><PropertyGroup>
<MainSource>App.dpr</MainSource><Config>$(MissingConfig)</Config><Platform>$(MissingPlatform)</Platform>
</PropertyGroup></Project>"#,
    );
    let client_context = discover(
        &client_main,
        client_root.path(),
        &ProjectOptions {
            build_config: Some("Debug".to_string()),
            platform: Some("Win32".to_string()),
            ..ProjectOptions::default()
        },
    );
    assert!(client_context.discovery_complete, "{client_context:?}");
    assert_eq!(
        (
            client_context.config.as_deref(),
            client_context.platform.as_deref()
        ),
        (Some("Debug"), Some("Win32"))
    );
    assert!(
        !client_context
            .warnings
            .iter()
            .any(|warning| warning.contains("MissingConfig") || warning.contains("MissingPlatform")),
        "immutable client globals expanded their XML RHS: {:?}",
        client_context.warnings
    );

    let file_root = tempfile::tempdir().unwrap();
    let file_main = file_root.path().join("App.dpr");
    write(&file_main, "program App; begin end.");
    write(
        &file_root.path().join("App.dproj"),
        r#"<Project><PropertyGroup>
<MainSource>App.dpr</MainSource><Config>$(MissingConfig)</Config><Platform>$(MissingPlatform)</Platform>
</PropertyGroup></Project>"#,
    );
    write(
        &file_root.path().join(".delphi-tools.local.toml"),
        "[properties]\nConfig = 'Release'\nPlatform = 'Win64'\n",
    );
    let file_context = discover(&file_main, file_root.path(), &ProjectOptions::default());
    assert!(file_context.discovery_complete, "{file_context:?}");
    assert_eq!(
        (
            file_context.config.as_deref(),
            file_context.platform.as_deref()
        ),
        (Some("Release"), Some("Win64"))
    );
    assert!(
        !file_context
            .warnings
            .iter()
            .any(|warning| warning.contains("MissingConfig") || warning.contains("MissingPlatform")),
        "immutable file globals expanded their XML RHS: {:?}",
        file_context.warnings
    );
}

#[test]
fn configured_empty_bds_is_not_replaced_by_project_xml() {
    let root = tempfile::tempdir().unwrap();
    let main = root.path().join("App.dpr");
    write(&main, "program App; begin end.");
    write(
        &root.path().join("App.dproj"),
        r#"<Project><PropertyGroup>
<MainSource>App.dpr</MainSource><BDS>ProjectValue</BDS><DCC_Namespace>$(BDS)</DCC_Namespace>
</PropertyGroup></Project>"#,
    );
    write(
        &root.path().join(".delphi-tools.local.toml"),
        "[properties]\nBDS = ''\n",
    );
    let session = OverrideSession::new(None);
    let context = ProjectContext::discover_with_overrides(
        &main,
        &[root.path().to_path_buf()],
        &options(),
        &session,
    )
    .unwrap();
    assert_eq!(
        context.overrides.properties.get("bds"),
        Some(&String::new())
    );
    assert!(context.unit_namespaces.is_empty());
}

#[test]
fn unrelated_unknown_metadata_remains_conservative_with_an_override() {
    let root = tempfile::tempdir().unwrap();
    let main = root.path().join("App.dpr");
    write(&main, "program App; begin end.");
    write(
        &root.path().join("App.dproj"),
        r#"<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup>
<Import Project="missing.optset" />
<PropertyGroup><DCC_Namespace>$(Unconfigured)</DCC_Namespace></PropertyGroup></Project>"#,
    );
    write(
        &root.path().join(".delphi-tools.local.toml"),
        "[properties]\nBDS = 'Expected'\n",
    );
    let session = OverrideSession::new(None);
    let context = ProjectContext::discover_with_overrides(
        &main,
        &[root.path().to_path_buf()],
        &options(),
        &session,
    )
    .unwrap();
    assert!(context.unit_namespaces.is_empty());
    assert!(!context.discovery_complete);
}

#[test]
fn automatic_candidate_uses_its_project_directory_override_scope() {
    let root = tempfile::tempdir().unwrap();
    let project_dir = root.path().join("projects");
    let source = project_dir.join("Main.dpr");
    write(&source, "program Main; begin end.");
    write(
        &project_dir.join("Main.dproj"),
        r#"<Project><PropertyGroup>
<MainSource>Main.dpr</MainSource><DCC_Namespace>$(BDS)</DCC_Namespace>
</PropertyGroup></Project>"#,
    );
    write(
        &root.path().join(".delphi-tools.local.toml"),
        "[properties]\nBDS = 'Workspace'\n",
    );
    write(
        &project_dir.join(".delphi-tools.local.toml"),
        "[properties]\nBDS = 'Candidate'\n",
    );
    let session = OverrideSession::new(None);
    let context = ProjectContext::discover_with_overrides(
        &source,
        &[root.path().to_path_buf()],
        &options(),
        &session,
    )
    .unwrap();
    assert!(
        context
            .unit_namespaces
            .iter()
            .any(|name| name == "Candidate")
    );
}

#[test]
fn incomplete_configured_candidate_does_not_establish_a_unique_owner() {
    let root = tempfile::tempdir().unwrap();
    let shared = root.path().join("Shared.pas");
    write(&shared, "unit Shared; interface implementation end.");
    write(&root.path().join("A.dpr"), "program A; begin end.");
    write(&root.path().join("B.dpr"), "program B; begin end.");
    write(
        &root.path().join("A.dproj"),
        r#"<Project>
<PropertyGroup><MainSource>A.dpr</MainSource></PropertyGroup>
<ItemGroup Condition="'$(Flavor)'=='Debug'"><DCCReference Include="Shared.pas"/></ItemGroup>
</Project>"#,
    );
    write(
        &root.path().join("B.dproj"),
        r#"<Project>
<PropertyGroup><MainSource>B.dpr</MainSource></PropertyGroup>
<ItemGroup Condition="Exists('$(UnknownRoot)')"><DCCReference Include="Shared.pas"/></ItemGroup>
</Project>"#,
    );
    write(
        &root.path().join(".delphi-tools.local.toml"),
        "[properties]\nFlavor = 'Debug'\n",
    );

    let context = discover(&shared, root.path(), &options());

    assert!(context.project_file.is_none(), "{context:?}");
    assert!(!context.discovery_complete, "{context:?}");
    assert!(
        context
            .metadata_files
            .iter()
            .any(|path| path == &root.path().join("A.dproj"))
    );
    assert!(
        context
            .metadata_files
            .iter()
            .any(|path| path == &root.path().join("B.dproj"))
    );
}

#[cfg(unix)]
#[test]
fn case_distinct_external_project_does_not_inherit_workspace_overrides() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("repo/ws");
    let external = root.path().join("repo/WS/app");
    let source = external.join("Main.dpr");
    let project = external.join("Main.dproj");
    write(&source, "program Main; begin end.");
    write(
        &project,
        r#"<Project><PropertyGroup>
<MainSource>Main.dpr</MainSource><DCC_Namespace>ProjectOnly</DCC_Namespace>
</PropertyGroup></Project>"#,
    );
    write(
        &workspace.join(".delphi-tools.local.toml"),
        "[properties]\nBDS = 'WorkspaceOnly'\n",
    );

    let session = OverrideSession::new(None);
    let context = ProjectContext::discover_with_overrides(
        &source,
        std::slice::from_ref(&workspace),
        &ProjectOptions {
            project_file: Some(project),
            ..ProjectOptions::default()
        },
        &session,
    )
    .unwrap();

    assert!(context.discovery_complete, "{context:?}");
    assert!(context.overrides.properties.is_empty(), "{context:?}");
    assert_eq!(context.unit_namespaces, ["ProjectOnly"]);
}

#[test]
fn malformed_override_keeps_selected_project_context_incomplete() {
    let root = tempfile::tempdir().unwrap();
    let main = root.path().join("App.dpr");
    let project = root.path().join("App.dproj");
    let user_config = root.path().join("user.toml");
    write(&main, "program App; begin end.");
    write(
        &project,
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup></Project>",
    );
    write(&user_config, "[properties\nBDS = 'invalid'\n");

    let session = OverrideSession::new(Some(user_config.clone()));
    let context = ProjectContext::discover_with_overrides(
        &main,
        &[root.path().to_path_buf()],
        &ProjectOptions {
            project_file: Some(project.clone()),
            ..ProjectOptions::default()
        },
        &session,
    )
    .expect("selected project context should remain inspectable");

    assert_eq!(context.project_file, Some(project));
    assert!(!context.discovery_complete, "{context:?}");
    assert!(
        context
            .warnings
            .iter()
            .any(|warning| warning.contains(&user_config.display().to_string())),
        "selected context lost malformed override provenance: {:?}",
        context.warnings
    );
}

#[test]
fn nested_workspace_override_does_not_inherit_ancestor_properties() {
    let root = tempfile::tempdir().unwrap();
    let ancestor = root.path().join("repository");
    let nested = ancestor.join("nested");
    let source = nested.join("Main.pas");
    write(&source, "unit Main; interface implementation end.");
    write(
        &ancestor.join(".delphi-tools.local.toml"),
        "[properties]\nShared = 'ancestor'\nAncestorOnly = 'leak'\n",
    );
    write(
        &nested.join(".delphi-tools.local.toml"),
        "[properties]\nShared = 'nested'\nNestedOnly = 'selected'\n",
    );

    let session = OverrideSession::new(None);
    session.capture_workspace(&ancestor).unwrap();
    session.capture_workspace(&nested).unwrap();
    let context =
        ProjectContext::discover_with_overrides(&source, &[ancestor, nested], &options(), &session)
            .unwrap();

    assert_eq!(
        context
            .overrides
            .properties
            .get("shared")
            .map(String::as_str),
        Some("nested")
    );
    assert_eq!(
        context
            .overrides
            .properties
            .get("nestedonly")
            .map(String::as_str),
        Some("selected")
    );
    assert!(!context.overrides.properties.contains_key("ancestoronly"));
}

#[test]
fn malformed_nested_workspace_override_is_retained_with_its_provenance() {
    let root = tempfile::tempdir().unwrap();
    let ancestor = root.path().join("repository");
    let nested = ancestor.join("nested");
    let source = nested.join("Main.pas");
    let nested_config = nested.join(".delphi-tools.local.toml");
    write(&source, "unit Main; interface implementation end.");
    write(
        &ancestor.join(".delphi-tools.local.toml"),
        "[properties]\nAncestor = 'valid'\n",
    );
    write(&nested_config, "[properties\ninvalid = 'nested'\n");

    let session = OverrideSession::new(None);
    session.capture_workspace(&ancestor).unwrap();
    let _ = session.capture_workspace(&nested);
    let context =
        ProjectContext::discover_with_overrides(&source, &[ancestor, nested], &options(), &session)
            .unwrap();

    assert!(!context.discovery_complete, "{context:?}");
    assert!(
        context
            .warnings
            .iter()
            .any(|warning| warning.contains(&nested_config.display().to_string())),
        "nested workspace error was not retained: {:?}",
        context.warnings
    );
}

#[test]
fn malformed_project_override_is_retained_with_its_provenance() {
    let root = tempfile::tempdir().unwrap();
    let project_dir = root.path().join("project");
    let main = project_dir.join("App.dpr");
    let project = project_dir.join("App.dproj");
    let project_config = project_dir.join(".delphi-tools.local.toml");
    write(&main, "program App; begin end.");
    write(
        &project,
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup></Project>",
    );
    write(&project_config, "[properties\ninvalid = 'project'\n");

    let session = OverrideSession::new(None);
    let context = ProjectContext::discover_with_overrides(
        &main,
        &[root.path().to_path_buf()],
        &ProjectOptions {
            project_file: Some(project.clone()),
            ..ProjectOptions::default()
        },
        &session,
    )
    .unwrap();

    assert_eq!(context.project_file, Some(project));
    assert!(!context.discovery_complete, "{context:?}");
    assert!(
        context
            .warnings
            .iter()
            .any(|warning| warning.contains(&project_config.display().to_string())),
        "project error was not retained: {:?}",
        context.warnings
    );
}

#[test]
fn evaluates_legacy_property_groups_in_source_order_and_maps_explicit_units() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    let project_dir = root.join("Projects/Tool");
    let main = project_dir.join("Tool.dpr");
    let main_unit = project_dir.join("MainUnit.pas");
    let shared = root.join("Common/Shared.pas");
    let config = project_dir.join("Config.pas");
    write(&main_unit, "unit MainUnit; interface implementation end.");
    write(&shared, "unit SharedAlias; interface implementation end.");
    write(&config, "unit Config; interface implementation end.");
    write(
        &main,
        "program Tool; uses MainUnit in 'MainUnit.pas', SharedAlias in '..\\..\\Common\\Shared.pas'; begin end.",
    );
    write(
        &project_dir.join("Tool.dproj"),
        r#"<Project>
  <PropertyGroup>
    <MainSource>Tool.dpr</MainSource>
    <Config Condition="'$(Config)'==''">Release</Config>
  </PropertyGroup>
  <PropertyGroup Condition="'$(Config)'=='Base' or '$(Base)'!=''">
    <Base>true</Base>
  </PropertyGroup>
  <PropertyGroup Condition="'$(Config)'=='Release' or '$(Cfg_1)'!=''">
    <Cfg_1>true</Cfg_1>
    <Base>true</Base>
  </PropertyGroup>
  <PropertyGroup Condition="'$(Base)'!=''">
    <DCC_Define>NOSF;$(DCC_Define)</DCC_Define>
    <DCC_UnitSearchPath>..\..\Common;$(DCC_UnitSearchPath)</DCC_UnitSearchPath>
    <DCC_UnitAlias>WinTypes=Windows;$(DCC_UnitAlias)</DCC_UnitAlias>
    <DCC_Namespace>System;$(DCC_Namespace)</DCC_Namespace>
  </PropertyGroup>
  <PropertyGroup Condition="'$(Cfg_1)'!=''">
    <DCC_Define>RELEASE;$(DCC_Define)</DCC_Define>
  </PropertyGroup>
  <ItemGroup>
    <DCCReference Include="Config.pas" />
    <DCCReference Include="..\..\Common\Shared.pas" />
  </ItemGroup>
</Project>"#,
    );

    let context = discover(&main_unit, root, &options());

    assert_eq!(context.project_file, Some(project_dir.join("Tool.dproj")));
    assert_eq!(context.main_source, Some(main.clone()));
    assert_eq!(context.config.as_deref(), Some("Release"));
    assert_eq!(context.defines, ["RELEASE", "NOSF"]);
    assert_eq!(context.unit_namespaces, ["System"]);
    assert_eq!(
        context.unit_aliases.get("WinTypes"),
        Some(&"Windows".to_string())
    );
    assert_eq!(
        context.explicit_units.get("mainunit"),
        Some(&vec![main_unit.clone()])
    );
    assert_eq!(
        context.explicit_units.get("sharedalias"),
        Some(&vec![shared.clone()])
    );
    assert_eq!(
        context.explicit_units.get("config"),
        Some(&vec![config.clone()])
    );
    assert!(context.search_paths.contains(&root.join("Common")));
}

#[test]
fn honors_explicit_debug_config_and_reads_local_optset_imports() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    let project_dir = root.join("App");
    let main = project_dir.join("App.dpr");
    let debug_units = project_dir.join("DebugUnits");
    write(&main, "program App; begin end.");
    fs::create_dir_all(&debug_units).expect("create debug unit path");
    write(
        &project_dir.join("common.optset"),
        r#"<Project>
  <PropertyGroup Condition="('$(Config)'=='Debug') And ('$(Platform)'=='Win32')">
    <DCC_Define>DEBUG;$(DCC_Define)</DCC_Define>
    <DCC_UnitSearchPath>DebugUnits;$(DCC_UnitSearchPath)</DCC_UnitSearchPath>
  </PropertyGroup>
</Project>"#,
    );
    write(
        &project_dir.join("App.dproj"),
        r#"<Project>
  <PropertyGroup>
    <MainSource>App.dpr</MainSource>
    <Config Condition="'$(Config)'==''">Release</Config>
  </PropertyGroup>
  <PropertyGroup Condition="'$(Config)'=='Debug'">
    <Cfg_2>true</Cfg_2>
  </PropertyGroup>
  <Import Project="common.optset" />
</Project>"#,
    );

    let context = discover(
        &main,
        root,
        &ProjectOptions {
            build_config: Some("Debug".to_string()),
            platform: Some("Win32".to_string()),
            ..ProjectOptions::default()
        },
    );

    assert_eq!(context.config.as_deref(), Some("Debug"));
    assert_eq!(context.platform.as_deref(), Some("Win32"));
    assert_eq!(context.defines, ["DEBUG"]);
    assert!(context.search_paths.contains(&debug_units));
    assert!(
        context.warnings.is_empty(),
        "unexpected warnings: {:?}",
        context.warnings
    );
}

#[test]
fn preserves_project_ambiguity_and_allows_explicit_project_override() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    let file = root.join("src/Unit.pas");
    write(&file, "unit Unit; interface implementation end.");
    write(
        &root.join("src/One.dproj"),
        "<Project><PropertyGroup><MainSource>One.dpr</MainSource></PropertyGroup></Project>",
    );
    write(
        &root.join("src/Two.dproj"),
        "<Project><PropertyGroup><MainSource>Two.dpr</MainSource></PropertyGroup></Project>",
    );

    let ambiguous = discover(&file, root, &options());
    assert!(ambiguous.project_file.is_none());
    assert!(
        ambiguous
            .warnings
            .iter()
            .any(|warning| warning.contains("multiple project files"))
    );

    let selected = discover(
        &file,
        root,
        &ProjectOptions {
            project_file: Some(PathBuf::from(r"src\Two.dproj")),
            ..ProjectOptions::default()
        },
    );
    assert_eq!(selected.project_file, Some(root.join("src/Two.dproj")));
}

#[test]
fn normalizes_case_insensitive_paths_and_rejects_unknown_windows_paths() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    let project_dir = root.join("Project");
    let main = project_dir.join("Main.dpr");
    let shared = root.join("Common/Shared.pas");
    write(&main, "program Main; begin end.");
    write(&shared, "unit Shared; interface implementation end.");
    write(
        &project_dir.join("Main.dproj"),
        r#"<Project>
  <PropertyGroup>
    <MainSource>main.dpr</MainSource>
    <DCC_UnitSearchPath>..\common;C:\Windows\Never;$(MissingPath)\also-never;$(DCC_UnitSearchPath)</DCC_UnitSearchPath>
  </PropertyGroup>
  <ItemGroup><DCCReference Include="..\COMMON\SHARED.PAS" /></ItemGroup>
</Project>"#,
    );

    let context = discover(&project_dir.join("MAIN.DPR"), root, &options());
    assert_eq!(context.main_source, Some(main));
    assert!(context.search_paths.contains(&root.join("Common")));
    assert_eq!(context.explicit_units.get("shared"), Some(&vec![shared]));
    assert!(
        context
            .warnings
            .iter()
            .any(|warning| warning.contains("Windows path"))
    );
    assert!(context.warnings.iter().any(|warning| {
        warning == "Windows path in DCC_UnitSearchPath is unavailable on Linux and was omitted: C:\\Windows\\Never"
    }));
    assert!(
        context
            .warnings
            .iter()
            .any(|warning| warning.contains("MissingPath"))
    );
}

#[test]
fn parses_only_real_dpr_unit_clauses_not_comments_or_strings() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    let project_dir = root.join("App");
    let main = project_dir.join("App.dpr");
    let real = project_dir.join("Actual.pas");
    let fake = project_dir.join("Fake.pas");
    write(&real, "unit Actual; interface implementation end.");
    write(&fake, "unit Fake; interface implementation end.");
    write(
        &main,
        r#"program App;
const Text = 'uses Fake in ''Fake.pas'';';
{ uses Fake in 'Fake.pas'; }
(* contains Fake in 'Fake.pas'; *)
uses Actual in 'Actual.pas';
begin
end."#,
    );
    write(
        &project_dir.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup></Project>",
    );

    let context = discover(&main, root, &options());
    assert_eq!(context.explicit_units.get("actual"), Some(&vec![real]));
    assert!(!context.explicit_units.contains_key("fake"));
}

#[test]
fn standalone_context_uses_only_bounded_explicit_source_paths() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    let file = root.join("src/Unit.pas");
    let source = root.join("Sources");
    write(&file, "unit Unit; interface implementation end.");
    fs::create_dir_all(&source).expect("create source path");

    let context = discover(
        &file,
        root,
        &ProjectOptions {
            source_paths: vec!["Sources".to_string()],
            ..ProjectOptions::default()
        },
    );

    assert!(context.project_file.is_none());
    assert!(context.main_source.is_none());
    assert!(context.search_paths.contains(&root.join("src")));
    assert!(context.search_paths.contains(&source));
}

#[test]
fn imported_optset_cycles_are_bounded_and_reported() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    let main = root.join("App.dpr");
    write(&main, "program App; begin end.");
    write(
        &root.join("App.dproj"),
        r#"<Project>
  <PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup>
  <Import Project="one.optset" />
</Project>"#,
    );
    write(
        &root.join("one.optset"),
        r#"<Project>
  <PropertyGroup><DCC_Define>ONE;$(DCC_Define)</DCC_Define></PropertyGroup>
  <Import Project="two.optset" />
</Project>"#,
    );
    write(
        &root.join("two.optset"),
        r#"<Project>
  <PropertyGroup><DCC_Define>TWO;$(DCC_Define)</DCC_Define></PropertyGroup>
  <Import Project="one.optset" />
</Project>"#,
    );

    let context = discover(&main, root, &options());
    assert!(context.defines.contains(&"ONE".to_string()));
    assert!(context.defines.contains(&"TWO".to_string()));
    assert!(
        context
            .warnings
            .iter()
            .any(|warning| warning.contains("cycle"))
    );
}

#[test]
fn uses_option_style_optsets_and_falls_back_to_project_stem_main_source() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    let project = root.join("App.dproj");
    let main = root.join("App.dpr");
    let units = root.join("Units");
    write(&main, "program App; begin end.");
    fs::create_dir_all(&units).expect("create units directory");
    write(
        &root.join("common.optset"),
        r#"<Options>
  <Option Name="DCC_Define">OPTSET;$(DCC_Define)</Option>
  <Option Name="DCC_UnitSearchPath">Units;$(DCC_UnitSearchPath)</Option>
</Options>"#,
    );
    write(
        &project,
        r#"<Project>
  <Import Project="common.optset" />
</Project>"#,
    );

    let context = discover(&root.join("Unit.pas"), root, &options());
    assert_eq!(context.main_source, Some(main));
    assert_eq!(context.defines, ["OPTSET"]);
    assert!(context.search_paths.contains(&units));
}

#[test]
fn known_missing_optset_import_is_retained_when_exists_guard_is_false() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    let main = root.join("App.dpr");
    let import = root.join("Mappings.optset");
    write(&main, "program App; begin end.");
    write(
        &root.join("App.dproj"),
        r#"<Project>
  <PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup>
  <Import Project="Mappings.optset" Condition="Exists('Mappings.optset')" />
</Project>"#,
    );

    let missing = discover(&main, root, &options());
    assert!(
        missing.metadata_files.iter().any(|path| path == &import),
        "known missing import was not retained: {missing:?}"
    );
    assert!(missing.defines.is_empty());

    write(
        &import,
        "<Project><PropertyGroup><DCC_Define>CREATED</DCC_Define></PropertyGroup></Project>",
    );
    let restored = discover(&main, root, &options());
    assert_eq!(restored.defines, ["CREATED"]);
}

#[test]
fn unresolved_optset_import_path_is_not_retained_or_guessed() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    let main = root.join("App.dpr");
    write(&main, "program App; begin end.");
    write(
        &root.join("App.dproj"),
        r#"<Project>
  <PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup>
  <Import Project="$(Unavailable)Mappings.optset" />
</Project>"#,
    );

    let context = discover(&main, root, &options());
    assert!(
        !context
            .metadata_files
            .iter()
            .any(|path| path.to_string_lossy().contains("$(Unavailable)"))
    );
    assert!(context.defines.is_empty());
    assert!(
        context
            .warnings
            .iter()
            .any(|warning| warning.contains("Unavailable"))
    );
}

#[test]
fn inactive_xml_ancestors_do_not_apply_references_or_target_properties() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    write(root.join("App.dpr").as_path(), "program App; begin end.");
    write(
        root.join("Other/U.pas").as_path(),
        "unit U; interface implementation end.",
    );
    write(
        root.join("App.dproj").as_path(),
        r#"<Project>
  <PropertyGroup><MainSource>App.dpr</MainSource><Config>Debug</Config></PropertyGroup>
  <ItemGroup Condition="'$(Config)'=='Release'"><DCCReference Include="Other/U.pas"/></ItemGroup>
  <Target Name="Never" Condition="'a'=='b'">
    <PropertyGroup><DCC_Define>NEVER</DCC_Define></PropertyGroup>
    <ItemGroup><DCCReference Include="Other/U.pas"/></ItemGroup>
  </Target>
</Project>"#,
    );

    let context = discover(&root.join("App.dpr"), root, &options());
    assert!(
        context.explicit_units.is_empty(),
        "inactive XML applied: {context:?}"
    );
    assert!(
        context.defines.is_empty(),
        "Target property applied: {context:?}"
    );
    assert!(
        context
            .warnings
            .iter()
            .any(|warning| warning.contains("Target")),
        "ignored Target was not reported: {context:?}"
    );
}

#[test]
fn unresolved_condition_paths_are_unknown_and_never_guessed() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    write(root.join("App.dpr").as_path(), "program App; begin end.");
    fs::create_dir(root.join("units")).expect("create units directory");
    write(
        root.join("App.dproj").as_path(),
        r#"<Project>
  <PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup>
  <PropertyGroup Condition="Exists('$(Unavailable)units')"><DCC_Define>GUESSED</DCC_Define></PropertyGroup>
</Project>"#,
    );

    let context = discover(&root.join("App.dpr"), root, &options());
    assert!(
        context.defines.is_empty(),
        "unknown path guessed: {context:?}"
    );
    assert!(!context.warnings.is_empty());
}

#[test]
fn tainted_condition_comparisons_remain_unknown() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    write(root.join("App.dpr").as_path(), "program App; begin end.");
    write(
        root.join("App.dproj").as_path(),
        r#"<Project>
  <PropertyGroup><MainSource>App.dpr</MainSource><Root>$(Unavailable)</Root></PropertyGroup>
  <PropertyGroup Condition="'$(Root)'!=''"><DCC_Define>GUESSED</DCC_Define></PropertyGroup>
</Project>"#,
    );

    let context = discover(&root.join("App.dpr"), root, &options());
    assert!(
        context.defines.is_empty(),
        "tainted comparison guessed: {context:?}"
    );
}

#[test]
fn ambiguous_workspace_root_never_allows_project_discovery_outside_it() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    write(
        root.join("ws/src/U.pas").as_path(),
        "unit U; interface implementation end.",
    );
    fs::create_dir(root.join("WS")).expect("create case-collision directory");
    write(
        root.join("Outside.dpr").as_path(),
        "program Outside; begin end.",
    );
    write(
        root.join("Outside.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>Outside.dpr</MainSource></PropertyGroup></Project>",
    );

    let session = OverrideSession::new(None);
    let context = ProjectContext::discover_with_overrides(
        &root.join("ws/src/U.pas"),
        &[root.join("ws")],
        &options(),
        &session,
    )
    .expect("discover project context");
    assert!(
        context.project_file.is_none(),
        "escaped workspace: {context:?}"
    );
}

#[test]
fn case_distinct_workspace_roots_keep_relative_project_ambiguity() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    write(
        root.join("ws/App.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup></Project>",
    );
    write(
        root.join("WS/App.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup></Project>",
    );
    write(
        root.join("WS/U.pas").as_path(),
        "unit U; interface implementation end.",
    );

    let session = OverrideSession::new(None);
    let context = ProjectContext::discover_with_overrides(
        &root.join("WS/U.pas"),
        &[root.join("ws"), root.join("WS")],
        &ProjectOptions {
            project_file: Some(PathBuf::from("App.dproj")),
            ..ProjectOptions::default()
        },
        &session,
    )
    .expect("discover project context");

    assert!(
        context.project_file.is_none(),
        "case-distinct roots collapsed: {context:?}"
    );
    assert!(
        context
            .warnings
            .iter()
            .any(|warning| { warning.contains("multiple explicit project files matched") })
    );
}

#[test]
fn nearest_unambiguous_dpr_can_own_a_different_member_unit_name() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    write(
        root.join("App.dpr").as_path(),
        "program App; uses U in 'src/U.pas'; begin end.",
    );
    write(
        root.join("src/U.pas").as_path(),
        "unit U; interface implementation end.",
    );

    let context = discover(&root.join("src/U.pas"), root, &options());
    assert_eq!(context.project_file, Some(root.join("App.dpr")));
}

#[test]
fn ancestor_dproj_beats_nested_dpr_for_unit_context() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    write(
        root.join("App.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>src/App.dpr</MainSource><DCC_Define>PROJECT_CONTEXT</DCC_Define></PropertyGroup></Project>",
    );
    write(
        root.join("src/App.dpr").as_path(),
        "program App; begin end.",
    );
    write(
        root.join("src/U.pas").as_path(),
        "unit U; interface implementation end.",
    );

    let context = discover(&root.join("src/U.pas"), root, &options());

    assert_eq!(context.project_file, Some(root.join("App.dproj")));
    assert_eq!(context.main_source, Some(root.join("src/App.dpr")));
    assert_eq!(context.defines, ["PROJECT_CONTEXT"]);
}

#[test]
fn imported_relative_references_use_the_main_project_directory() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    write(root.join("App.dpr").as_path(), "program App; begin end.");
    write(
        root.join("src/U.pas").as_path(),
        "unit U; interface implementation end.",
    );
    write(
        root.join("settings/src/U.pas").as_path(),
        "unit U; interface implementation end.",
    );
    write(
        root.join("settings/common.optset").as_path(),
        "<Project><ItemGroup><DCCReference Include=\"src/U.pas\"/></ItemGroup></Project>",
    );
    write(
        root.join("App.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup><Import Project=\"settings/common.optset\"/></Project>",
    );

    let context = discover(&root.join("App.dpr"), root, &options());
    assert_eq!(
        context.explicit_units.get("u"),
        Some(&vec![root.join("src/U.pas")])
    );
}

#[test]
fn explicit_config_and_platform_are_immutable_global_properties() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    write(root.join("App.dpr").as_path(), "program App; begin end.");
    write(
        root.join("App.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource><Config>Release</Config><Platform>Win32</Platform></PropertyGroup></Project>",
    );

    let session = OverrideSession::new(None);
    let context = ProjectContext::discover_with_overrides(
        &root.join("App.dpr"),
        &[root.to_path_buf()],
        &ProjectOptions {
            build_config: Some("Debug".to_string()),
            platform: Some("Win64".to_string()),
            ..ProjectOptions::default()
        },
        &session,
    )
    .expect("discover project context");
    assert_eq!(
        (context.config.as_deref(), context.platform.as_deref()),
        (Some("Debug"), Some("Win64"))
    );
}

#[test]
fn property_expansion_has_per_value_and_aggregate_budgets() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    write(root.join("App.dpr").as_path(), "program App; begin end.");
    let mut xml = String::from(
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource><DCC_Define>x</DCC_Define>",
    );
    for _ in 0..24 {
        xml.push_str("<DCC_Define>$(DCC_Define)$(DCC_Define)</DCC_Define>");
    }
    xml.push_str("</PropertyGroup></Project>");
    write(root.join("App.dproj").as_path(), &xml);

    let context = discover(&root.join("App.dpr"), root, &options());
    let expanded_bytes: usize = context.defines.iter().map(String::len).sum();
    assert!(
        expanded_bytes <= 4 * 1024 * 1024 || !context.warnings.is_empty(),
        "{expanded_bytes} bytes of XML silently expanded to {expanded_bytes} bytes"
    );
}

#[test]
fn seeded_property_budget_warnings_identify_client_provenance() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    let override_file = root.join(".delphi-tools.local.toml");
    write(root.join("App.dpr").as_path(), "program App; begin end.");
    write(
        root.join("App.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup></Project>",
    );
    write(&override_file, "[properties]\nConfig = 'file default'\n");

    let context = discover(
        &root.join("App.dpr"),
        root,
        &ProjectOptions {
            build_config: Some("x".repeat(16 * 1024 * 1024 + 1)),
            ..ProjectOptions::default()
        },
    );

    assert!(!context.discovery_complete);
    assert!(
        context.warnings.iter().any(|warning| {
            warning.contains("configured Delphi override properties exceed")
                && warning.contains("client initialization options")
        }),
        "aggregate seeded-property warning lost provenance: {:?}",
        context.warnings
    );
    assert!(
        context.warnings.iter().any(|warning| {
            warning.contains("configured Delphi property config")
                && warning.contains("client initialization options")
                && !warning.contains(&override_file.display().to_string())
        }),
        "client property was attributed to the override file: {:?}",
        context.warnings
    );
}

#[test]
fn legacy_project_keeps_release_metadata_and_reference_paths() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path().to_path_buf();
    let project = root.join("Projects/Tools/WebQueryExporter");
    write(
        &project.join("Config.pas"),
        "unit Config; interface implementation end.",
    );
    write(
        &root.join("Projects/Chaindrive/Librairies/hartlib.pas"),
        "unit hartlib; interface implementation end.",
    );
    write(
        &project.join("WebQuery.dpr"),
        "program WebQuery; uses Config in 'Config.pas', hartlib in '..\\..\\Chaindrive\\Librairies\\hartlib.pas'; begin end.",
    );
    write(
        &project.join("WebQuery.dproj"),
        r#"<Project xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
          <PropertyGroup>
            <MainSource>WebQuery.dpr</MainSource>
            <Config Condition="'$(Config)'==''">Release</Config>
          </PropertyGroup>
          <PropertyGroup Condition="'$(Config)'=='Release' or '$(Cfg_1)'!=''">
            <Cfg_1>true</Cfg_1><Base>true</Base>
          </PropertyGroup>
          <PropertyGroup Condition="'$(Base)'!=''">
            <DCC_Define>NOSF;$(DCC_Define)</DCC_Define><DCC_Platform>x86</DCC_Platform>
          </PropertyGroup>
          <PropertyGroup Condition="'$(Cfg_1)'!=''">
            <DCC_Define>RELEASE;$(DCC_Define)</DCC_Define>
          </PropertyGroup>
          <ItemGroup>
            <DelphiCompile Include="WebQuery.dpr"><MainSource>MainSource</MainSource></DelphiCompile>
            <DCCReference Include="Config.pas"/>
            <DCCReference Include="..\..\Chaindrive\Librairies\hartlib.pas"/>
          </ItemGroup>
        </Project>"#,
    );
    let session = OverrideSession::new(None);
    let context = ProjectContext::discover_with_overrides(
        &root.join("Projects/Tools/WebQueryExporter/Config.pas"),
        std::slice::from_ref(&root),
        &ProjectOptions::default(),
        &session,
    )
    .expect("discover legacy project");
    assert_eq!(
        context.main_source,
        Some(root.join("Projects/Tools/WebQueryExporter/WebQuery.dpr"))
    );
    assert_eq!(context.defines, ["RELEASE", "NOSF"]);
    assert_eq!(context.platform.as_deref(), Some("x86"));
    assert_eq!(
        context.explicit_units.get("hartlib"),
        Some(&vec![
            root.join("Projects/Chaindrive/Librairies/hartlib.pas")
        ])
    );
}

#[test]
fn evaluates_dcc_use_package_names_in_declared_order_without_duplicates() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    write(root.join("App.dpr").as_path(), "program App; begin end.");
    write(
        root.join("App.dproj").as_path(),
        r#"<Project>
  <PropertyGroup>
    <MainSource>App.dpr</MainSource>
    <DCC_UsePackage>FirstPkg; MultidevD10;firstpkg;SecondPkg</DCC_UsePackage>
  </PropertyGroup>
</Project>"#,
    );

    let context = discover(&root.join("App.dpr"), root, &options());

    assert_eq!(context.packages, ["firstpkg", "multidevd10", "secondpkg"]);
}

#[test]
fn ordinary_undefined_configuration_properties_are_empty() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    write(root.join("App.dpr").as_path(), "program App; begin end.");
    write(
        root.join("App.dproj").as_path(),
        r#"<Project>
  <PropertyGroup>
    <MainSource>App.dpr</MainSource>
    <Base>true</Base>
  </PropertyGroup>
  <PropertyGroup Condition="'$(Config)'=='Debug' or '$(Cfg_1)'!=''">
    <Cfg_1>true</Cfg_1>
  </PropertyGroup>
  <PropertyGroup Condition="('$(Platform)'=='Win32' and '$(Cfg_1)'=='true') or '$(Cfg_1_Win32)'!=''">
    <DCC_Define>DEBUG</DCC_Define>
  </PropertyGroup>
  <PropertyGroup Condition="'$(Cfg_2)'!=''">
    <DCC_Define>RELEASE</DCC_Define>
  </PropertyGroup>
</Project>"#,
    );

    let context = discover(
        &root.join("App.dpr"),
        root,
        &ProjectOptions {
            build_config: Some("Debug".to_string()),
            platform: Some("Win32".to_string()),
            ..ProjectOptions::default()
        },
    );

    assert_eq!(context.defines, ["DEBUG"]);
    assert!(
        context.discovery_complete,
        "unexpected incomplete context: {context:?}"
    );
    assert!(
        context
            .warnings
            .iter()
            .all(|warning| !warning.contains("Cfg_2") && !warning.contains("Cfg_1_Win32")),
        "ordinary missing configuration properties stayed unknown: {:?}",
        context.warnings
    );
}

#[test]
fn properties_from_unknown_prior_conditions_remain_unknown() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    write(root.join("App.dpr").as_path(), "program App; begin end.");
    write(
        root.join("App.dproj").as_path(),
        r#"<Project>
  <PropertyGroup>
    <MainSource>App.dpr</MainSource>
  </PropertyGroup>
  <PropertyGroup Condition="Exists('$(Unavailable)')">
    <Derived>true</Derived>
  </PropertyGroup>
  <PropertyGroup Condition="'$(Derived)'!=''">
    <DCC_Define>GUESSED</DCC_Define>
  </PropertyGroup>
</Project>"#,
    );

    let context = discover(&root.join("App.dpr"), root, &options());

    assert!(
        context.defines.is_empty(),
        "unknown property was guessed: {context:?}"
    );
    assert!(
        !context.discovery_complete,
        "unknown property was treated as complete"
    );
    assert!(
        context
            .warnings
            .iter()
            .any(|warning| warning.contains("unknown project condition")),
        "unknown condition was not reported: {:?}",
        context.warnings
    );
}

#[test]
fn automatic_selection_uses_the_unique_proven_source_owner() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    let source = root.join("src/Shared.pas");
    write(&source, "unit Shared; interface implementation end.");
    write(
        root.join("Other.pas").as_path(),
        "unit Other; interface implementation end.",
    );
    write(root.join("A.dpr").as_path(), "program A; begin end.");
    write(root.join("B.dpr").as_path(), "program B; begin end.");
    write(
        root.join("A.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>A.dpr</MainSource><DCCReference Include=\"Other.pas\"/></PropertyGroup></Project>",
    );
    write(
        root.join("B.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>B.dpr</MainSource><DCCReference Include=\"src/Shared.pas\"/></PropertyGroup></Project>",
    );

    let context = discover(&source, root, &options());

    assert_eq!(context.project_file, Some(root.join("B.dproj")));
    assert!(
        context.discovery_complete,
        "unique owner was not complete: {context:?}"
    );
    assert!(
        context
            .metadata_files
            .iter()
            .any(|path| path == &root.join("A.dproj"))
    );
    assert!(
        context
            .metadata_files
            .iter()
            .any(|path| path == &root.join("B.dproj"))
    );
}

#[test]
fn automatic_selection_refuses_shared_source_owners_even_through_aliases() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    let source = root.join("src/Shared.pas");
    write(&source, "unit Shared; interface implementation end.");
    write(root.join("A.dpr").as_path(), "program A; begin end.");
    write(root.join("B.dpr").as_path(), "program B; begin end.");
    write(
        root.join("A.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>A.dpr</MainSource><DCCReference Include=\"src\\Shared.pas\"/></PropertyGroup></Project>",
    );
    write(
        root.join("B.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>B.dpr</MainSource><DCCReference Include=\"./src/../src/Shared.pas\"/></PropertyGroup></Project>",
    );

    let context = discover(&source, root, &options());

    assert!(
        context.project_file.is_none(),
        "shared ownership was guessed: {context:?}"
    );
    assert!(!context.discovery_complete);
    assert!(
        context
            .warnings
            .iter()
            .any(|warning| warning.contains("multiple project files")),
        "shared ownership was not reported: {:?}",
        context.warnings
    );
    assert!(
        context
            .metadata_files
            .iter()
            .any(|path| path == &root.join("A.dproj"))
    );
    assert!(
        context
            .metadata_files
            .iter()
            .any(|path| path == &root.join("B.dproj"))
    );
}

#[test]
fn incomplete_candidate_metadata_is_not_exclusion_proof() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    let source = root.join("src/Shared.pas");
    write(&source, "unit Shared; interface implementation end.");
    write(
        root.join("Owner.dpr").as_path(),
        "program Owner; begin end.",
    );
    write(
        root.join("Owner.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>Owner.dpr</MainSource><DCCReference Include=\"src/Shared.pas\"/></PropertyGroup></Project>",
    );
    write(
        root.join("Unknown.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>Unknown.dpr</MainSource><DCCReference Include=\"$(Unavailable)/src/Shared.pas\"/></PropertyGroup></Project>",
    );

    let context = discover(&source, root, &options());

    assert!(
        context.project_file.is_none(),
        "incomplete candidate was excluded: {context:?}"
    );
    assert!(!context.discovery_complete);
    assert!(
        context
            .metadata_files
            .iter()
            .any(|path| path == &root.join("Unknown.dproj"))
    );
}

#[test]
fn missing_compiled_references_do_not_invalidate_source_context() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    write(root.join("App.dpr").as_path(), "program App; begin end.");
    write(
        root.join("App.dproj").as_path(),
        r#"<Project>
  <PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup>
  <ItemGroup><DCCReference Include="missing.dcp" /></ItemGroup>
</Project>"#,
    );

    let context = discover(&root.join("App.dpr"), root, &options());

    assert!(
        context.discovery_complete,
        "missing DCP poisoned context: {context:?}"
    );
    assert!(context.explicit_units.is_empty());
    assert!(
        context
            .warnings
            .iter()
            .all(|warning| !warning.contains("missing.dcp")),
        "compiled-only reference was treated as source: {:?}",
        context.warnings
    );
}

#[test]
fn missing_source_references_remain_incomplete() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    write(root.join("App.dpr").as_path(), "program App; begin end.");
    write(
        root.join("App.dproj").as_path(),
        r#"<Project>
  <PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup>
  <ItemGroup>
    <DCCReference Include="missing.pas" />
    <DCCReference Include="missing.dpk" />
    <DCCReference Include="missing.inc" />
  </ItemGroup>
</Project>"#,
    );

    let context = discover(&root.join("App.dpr"), root, &options());

    assert!(
        !context.discovery_complete,
        "missing source references were ignored: {context:?}"
    );
    for extension in ["missing.pas", "missing.dpk", "missing.inc"] {
        assert!(
            context
                .warnings
                .iter()
                .any(|warning| warning.contains(extension)),
            "missing source reference {extension} was not reported: {:?}",
            context.warnings
        );
    }
}

#[test]
fn evaluates_inherited_include_paths_without_adding_them_to_unit_search_paths() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    let project_include = root.join("project-includes");
    let optset_include = root.join("optset-includes");
    fs::create_dir_all(&project_include).expect("create project include path");
    fs::create_dir_all(&optset_include).expect("create optset include path");
    write(root.join("App.dpr").as_path(), "program App; begin end.");
    write(
        root.join("common.optset").as_path(),
        "<Project><PropertyGroup><DCC_IncludePath>optset-includes;$(DCC_IncludePath)</DCC_IncludePath><DCC_UnitSearchPath>units;$(DCC_UnitSearchPath)</DCC_UnitSearchPath></PropertyGroup></Project>",
    );
    write(
        root.join("App.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup><Import Project=\"common.optset\"/><PropertyGroup><DCC_IncludePath>project-includes;$(DCC_IncludePath)</DCC_IncludePath></PropertyGroup></Project>",
    );

    let context = discover(&root.join("App.dpr"), root, &options());

    assert_eq!(
        context.include_paths,
        vec![project_include.clone(), optset_include.clone()]
    );
    assert!(!context.search_paths.contains(&project_include));
    assert!(!context.search_paths.contains(&optset_include));
    assert!(context.search_paths.contains(&root.join("units")));
}

#[test]
fn bare_uses_is_not_exclusion_proof_for_automatic_selection() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    write(
        root.join("src/Shared.pas").as_path(),
        "unit Shared; interface implementation end.",
    );
    write(root.join("A.dpr").as_path(), "program A; begin end.");
    write(
        root.join("B.dpr").as_path(),
        "program B; uses Shared; begin end.",
    );
    write(
        root.join("A.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>A.dpr</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"src/Shared.pas\"/></ItemGroup></Project>",
    );
    write(
        root.join("B.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>B.dpr</MainSource><DCC_UnitSearchPath>src</DCC_UnitSearchPath></PropertyGroup></Project>",
    );

    let context = discover(&root.join("src/Shared.pas"), root, &options());

    assert!(
        context.project_file.is_none(),
        "bare uses were excluded: {context:?}"
    );
    assert!(!context.discovery_complete);
}

#[test]
fn transitive_bare_uses_are_not_exclusion_proof_for_automatic_selection() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    write(
        root.join("src/Shared.pas").as_path(),
        "unit Shared; interface implementation end.",
    );
    write(
        root.join("Middle.pas").as_path(),
        "unit Middle; interface uses Shared; implementation end.",
    );
    write(root.join("A.dpr").as_path(), "program A; begin end.");
    write(
        root.join("B.dpr").as_path(),
        "program B; uses Middle in 'Middle.pas'; begin end.",
    );
    write(
        root.join("A.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>A.dpr</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"src/Shared.pas\"/></ItemGroup></Project>",
    );
    write(
        root.join("B.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>B.dpr</MainSource></PropertyGroup></Project>",
    );

    let context = discover(&root.join("src/Shared.pas"), root, &options());

    assert!(
        context.project_file.is_none(),
        "transitive bare uses were excluded: {context:?}"
    );
    assert!(!context.discovery_complete);
}

#[test]
fn include_provided_membership_is_not_exclusion_proof_for_automatic_selection() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    write(
        root.join("src/Shared.pas").as_path(),
        "unit Shared; interface implementation end.",
    );
    write(root.join("B.inc").as_path(), "uses Shared;");
    write(root.join("A.dpr").as_path(), "program A; begin end.");
    write(
        root.join("B.dpr").as_path(),
        "program B; {$I B.inc} begin end.",
    );
    write(
        root.join("A.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>A.dpr</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"src/Shared.pas\"/></ItemGroup></Project>",
    );
    write(
        root.join("B.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>B.dpr</MainSource><DCC_UnitSearchPath>src</DCC_UnitSearchPath></PropertyGroup></Project>",
    );

    let context = discover(&root.join("src/Shared.pas"), root, &options());

    assert!(
        context.project_file.is_none(),
        "include-provided membership was excluded: {context:?}"
    );
    assert!(!context.discovery_complete);
}

#[cfg(unix)]
#[test]
fn symlink_source_is_not_exclusion_proof_for_automatic_selection() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    write(
        root.join("src/Shared.pas").as_path(),
        "unit Shared; interface implementation end.",
    );
    write(root.join("A.dpr").as_path(), "program A; begin end.");
    write(root.join("B.dpr").as_path(), "program B; begin end.");
    write(
        root.join("A.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>A.dpr</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"src/Shared.pas\"/></ItemGroup></Project>",
    );
    write(
        root.join("B.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>B.dpr</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"alias/Shared.pas\"/></ItemGroup></Project>",
    );
    std::os::unix::fs::symlink(root.join("src"), root.join("alias")).expect("create source alias");

    let context = discover(&root.join("src/Shared.pas"), root, &options());

    assert!(
        context.project_file.is_none(),
        "symlink ownership was excluded: {context:?}"
    );
    assert!(!context.discovery_complete);
}

#[test]
fn unmodeled_environment_properties_are_unknown_in_both_comparison_directions() {
    for condition in ["'$(OS)'=='Windows_NT'", "'$(OS)'!='Windows_NT'"] {
        let temp = tempfile::tempdir().expect("temporary fixture");
        let root = temp.path();
        write(
            root.join("src/Shared.pas").as_path(),
            "unit Shared; interface implementation end.",
        );
        write(root.join("B.dpr").as_path(), "program B; begin end.");
        write(
            root.join("B.dproj").as_path(),
            &format!(
                "<Project><PropertyGroup><MainSource>B.dpr</MainSource></PropertyGroup><ItemGroup Condition=\"{condition}\"><DCCReference Include=\"src/Shared.pas\"/></ItemGroup></Project>"
            ),
        );

        let context = discover(
            &root.join("src/Shared.pas"),
            root,
            &ProjectOptions {
                project_file: Some(root.join("B.dproj")),
                ..ProjectOptions::default()
            },
        );

        assert!(
            !context.discovery_complete,
            "unmodeled OS input was treated as known for {condition}: {context:?}"
        );
    }
}

#[test]
fn metadata_limit_prevents_automatic_exclusion() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    write(
        root.join("src/Shared.pas").as_path(),
        "unit Shared; interface implementation end.",
    );
    write(root.join("A.dpr").as_path(), "program A; begin end.");
    write(root.join("B.dpr").as_path(), "program B; begin end.");
    write(
        root.join("A.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>A.dpr</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"src/Shared.pas\"/></ItemGroup></Project>",
    );
    let mut imports = String::new();
    for index in 0..64 {
        imports.push_str(&format!(
            "<Import Project=\"unused{index}.optset\" Condition=\"'a'=='b'\"/>"
        ));
    }
    imports.push_str("<Import Project=\"owner.optset\"/>");
    write(
        root.join("owner.optset").as_path(),
        "<Project><ItemGroup><DCCReference Include=\"src/Shared.pas\"/></ItemGroup></Project>",
    );
    write(
        root.join("B.dproj").as_path(),
        &format!(
            "<Project><PropertyGroup><MainSource>B.dpr</MainSource></PropertyGroup>{imports}</Project>"
        ),
    );

    let explicit = discover(
        &root.join("src/Shared.pas"),
        root,
        &ProjectOptions {
            project_file: Some(root.join("B.dproj")),
            ..ProjectOptions::default()
        },
    );
    assert!(
        !explicit.discovery_complete,
        "metadata cutoff was reported as complete: {explicit:?}"
    );
    assert!(
        explicit
            .warnings
            .iter()
            .any(|warning| warning.contains("metadata file limit")),
        "metadata cutoff warning was lost: {:?}",
        explicit.warnings
    );

    let automatic = discover(&root.join("src/Shared.pas"), root, &options());
    assert!(
        automatic.project_file.is_none() && !automatic.discovery_complete,
        "metadata cutoff allowed automatic selection: {automatic:?}"
    );
    assert!(
        automatic
            .warnings
            .iter()
            .any(|warning| warning.contains("metadata file limit")),
        "automatic cutoff warning was lost: {:?}",
        automatic.warnings
    );
}

#[cfg(unix)]
#[test]
fn case_distinct_candidate_is_preserved_in_the_ownership_readset() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    write(
        root.join("src/Shared.pas").as_path(),
        "unit Shared; interface implementation end.",
    );
    write(root.join("A.dpr").as_path(), "program A; begin end.");
    write(root.join("B.dpr").as_path(), "program B; begin end.");
    write(
        root.join("A.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>A.dpr</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"src/Shared.pas\"/></ItemGroup></Project>",
    );
    write(
        root.join("B.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>B.dpr</MainSource></PropertyGroup></Project>",
    );
    write(
        root.join("a.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>A.dpr</MainSource></PropertyGroup></Project>",
    );

    let before = discover(&root.join("src/Shared.pas"), root, &options());
    assert_eq!(before.project_file, Some(root.join("A.dproj")));
    assert!(
        before
            .metadata_files
            .iter()
            .any(|path| path == &root.join("a.dproj")),
        "case-distinct candidate was dropped: {before:?}"
    );

    write(
        root.join("a.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>A.dpr</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"src/Shared.pas\"/></ItemGroup></Project>",
    );
    let after = discover(&root.join("src/Shared.pas"), root, &options());
    assert!(
        after.project_file.is_none() && !after.discovery_complete,
        "mutated case-distinct candidate was not re-evaluated: {after:?}"
    );
}

#[test]
fn exists_dependencies_are_retained_in_the_ownership_readset() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    write(
        root.join("src/Shared.pas").as_path(),
        "unit Shared; interface implementation end.",
    );
    write(root.join("A.dpr").as_path(), "program A; begin end.");
    write(root.join("B.dpr").as_path(), "program B; begin end.");
    write(
        root.join("A.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>A.dpr</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"src/Shared.pas\"/></ItemGroup></Project>",
    );
    write(
        root.join("B.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>B.dpr</MainSource></PropertyGroup><ItemGroup Condition=\"Exists('enabled.flag')\"><DCCReference Include=\"src/Shared.pas\"/></ItemGroup></Project>",
    );

    let before = discover(&root.join("src/Shared.pas"), root, &options());
    assert_eq!(before.project_file, Some(root.join("A.dproj")));
    assert!(
        before
            .metadata_files
            .iter()
            .any(|path| path == &root.join("enabled.flag")),
        "missing Exists dependency was not retained: {before:?}"
    );

    write(root.join("enabled.flag").as_path(), "enabled");
    let after = discover(&root.join("src/Shared.pas"), root, &options());
    assert!(
        after.project_file.is_none() && !after.discovery_complete,
        "Exists dependency change did not invalidate ownership: {after:?}"
    );
}

#[test]
fn unknown_import_taints_later_property_conditions_until_reassignment() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    write(root.join("App.dpr").as_path(), "program App; begin end.");
    write(
        root.join("unknown.optset").as_path(),
        "<Project><PropertyGroup><Derived>true</Derived><Config>Release</Config><Platform>Win64</Platform></PropertyGroup></Project>",
    );
    write(
        root.join("App.dproj").as_path(),
        r#"<Project>
  <PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup>
  <Import Project="unknown.optset" Condition="Exists('$(Unavailable)')"/>
  <PropertyGroup Condition="'$(Derived)'==''"><DCC_Define>GUESSED</DCC_Define></PropertyGroup>
  <PropertyGroup><Derived>known</Derived><Config>Release</Config><Platform>Win64</Platform></PropertyGroup>
  <PropertyGroup Condition="'$(Derived)'=='known'"><DCC_Define>KNOWN</DCC_Define></PropertyGroup>
</Project>"#,
    );

    let context = discover(
        &root.join("App.dpr"),
        root,
        &ProjectOptions {
            build_config: Some("Debug".to_string()),
            platform: Some("Win32".to_string()),
            ..ProjectOptions::default()
        },
    );

    assert_eq!(context.defines, ["KNOWN"]);
    assert_eq!(context.config.as_deref(), Some("Debug"));
    assert_eq!(context.platform.as_deref(), Some("Win32"));
    assert!(!context.discovery_complete);
}

#[test]
fn automatic_selection_has_an_aggregate_candidate_probe_bound() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    write(
        root.join("src/Shared.pas").as_path(),
        "unit Shared; interface implementation end.",
    );
    write(
        root.join("Owner.dpr").as_path(),
        "program Owner; begin end.",
    );
    write(
        root.join("Owner.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>Owner.dpr</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"src/Shared.pas\"/></ItemGroup></Project>",
    );
    for index in 0..32 {
        let name = format!("Candidate{index}");
        write(
            root.join(format!("{name}.dpr")).as_path(),
            &format!("program {name}; begin end."),
        );
        write(
            root.join(format!("{name}.dproj")).as_path(),
            &format!(
                "<Project><PropertyGroup><MainSource>{name}.dpr</MainSource></PropertyGroup></Project>"
            ),
        );
    }

    let context = discover(&root.join("src/Shared.pas"), root, &options());

    assert!(
        context.project_file.is_none() && !context.discovery_complete,
        "candidate probe budget was not enforced: {context:?}"
    );
    assert!(
        context
            .warnings
            .iter()
            .any(|warning| warning.contains("candidate") && warning.contains("limit")),
        "candidate probe limit was not reported: {:?}",
        context.warnings
    );
}

#[test]
fn conditional_alternative_paths_are_not_exclusion_proof_for_automatic_selection() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    write(
        root.join("src/Shared.pas").as_path(),
        "unit Shared; interface implementation end.",
    );
    write(
        root.join("Other.pas").as_path(),
        "unit Other; interface implementation end.",
    );
    write(root.join("A.dpr").as_path(), "program A; begin end.");
    write(
        root.join("B.dpr").as_path(),
        "program B;\nuses\n  {$IFDEF OTHER}\n  Other in 'Other.pas'\n  {$ELSE}\n  Shared in 'src/Shared.pas'\n  {$ENDIF};\nbegin\nend.",
    );
    write(
        root.join("A.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>A.dpr</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"src/Shared.pas\"/></ItemGroup></Project>",
    );
    write(
        root.join("B.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>B.dpr</MainSource></PropertyGroup></Project>",
    );

    let context = discover(&root.join("src/Shared.pas"), root, &options());

    assert!(
        context.project_file.is_none() && !context.discovery_complete,
        "conditional alternative was treated as exhaustive ownership evidence: {context:?}"
    );
}

#[test]
fn project_defines_select_the_active_navigation_branch_without_inventing_versions() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    let main = root.join("Main.pas");
    let project = root.join("App.dproj");
    let source = "unit Main;\ninterface\n{$IFDEF FEATURE}\nconst Target = 1;\n{$ELSE}\nconst Target = 2;\n{$ENDIF}\nimplementation\nprocedure Run;\nbegin\n  WriteLn(Target);\nend;\nend.\n";
    write(&main, source);
    write(
        &project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_Define>FEATURE</DCC_Define></PropertyGroup></Project>",
    );

    let context = discover(&main, root, &options());
    assert_eq!(context.defines, ["FEATURE"]);

    let source_uri = Url::from_file_path(&main).expect("source URI");
    let mut workspace = Workspace::new(
        vec![root.to_path_buf()],
        WorkspaceOptions {
            project_file: Some(project),
            ..WorkspaceOptions::default()
        },
    );
    workspace
        .open_document(source_uri.clone(), source.to_owned(), 1)
        .expect("open project source");
    let locations = workspace.navigate(
        &source_uri,
        Position::new(10, 10),
        pascal_lsp::NavigationTarget::Declaration,
    );
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].range.start.line, 3);
}

#[test]
fn project_defines_select_the_active_local_rename_branch() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    let main = root.join("Main.pas");
    let project = root.join("App.dproj");
    let source = "unit Main;\ninterface\nimplementation\nprocedure Run;\nvar\n{$IFDEF FEATURE}\n  Target: Integer;\n{$ELSE}\n  Other: Integer;\n{$ENDIF}\nbegin\n  Target := 1;\nend;\nend.\n";
    write(&main, source);
    write(
        &project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_Define>FEATURE</DCC_Define></PropertyGroup></Project>",
    );

    let source_uri = Url::from_file_path(&main).expect("source URI");
    let mut workspace = Workspace::new(
        vec![root.to_path_buf()],
        WorkspaceOptions {
            project_file: Some(project),
            ..WorkspaceOptions::default()
        },
    );
    workspace
        .open_document(source_uri.clone(), source.to_owned(), 1)
        .expect("open project source");

    let edits = workspace
        .rename_edits(&source_uri, Position::new(11, 2), "RenamedTarget", false)
        .expect("project-selected local conditional rename succeeds");
    let document_edits = edits
        .changes
        .expect("rename uses unversioned changes")
        .remove(&source_uri)
        .expect("source edits are present");
    let mut edited_lines = document_edits
        .iter()
        .map(|edit| edit.range.start.line)
        .collect::<Vec<_>>();
    edited_lines.sort_unstable();
    assert_eq!(edited_lines, [6, 11]);
    assert!(
        document_edits
            .iter()
            .all(|edit| edit.new_text == "RenamedTarget")
    );
}

#[test]
fn changing_project_defines_reindexes_open_conditional_sources_conservatively() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    let main = root.join("Main.pas");
    let project = root.join("App.dproj");
    let source = "unit Main;\ninterface\n{$IFDEF FEATURE}\nconst Target = 1;\n{$ELSE}\nconst Target = 2;\n{$ENDIF}\nimplementation\nprocedure Run;\nbegin\n  WriteLn(Target);\nend;\nend.\n";
    write(&main, source);
    write(
        &project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_Define>FEATURE</DCC_Define></PropertyGroup></Project>",
    );

    let source_uri = Url::from_file_path(&main).expect("source URI");
    let project_uri = Url::from_file_path(&project).expect("project URI");
    let mut workspace = Workspace::new(vec![root.to_path_buf()], WorkspaceOptions::default());
    workspace
        .open_document(source_uri.clone(), source.to_owned(), 1)
        .expect("open project source");

    let before = workspace.navigate(
        &source_uri,
        Position::new(10, 10),
        pascal_lsp::NavigationTarget::Declaration,
    );
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].range.start.line, 3);

    write(
        &project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource><DCC_Define>OTHER</DCC_Define></PropertyGroup></Project>",
    );
    workspace.file_event(&project_uri, FileChange::Changed);

    let after = workspace.navigate(
        &source_uri,
        Position::new(10, 10),
        pascal_lsp::NavigationTarget::Declaration,
    );
    assert!(
        after.is_empty(),
        "the newly unknown project branch must not reuse the old projection"
    );
}

#[cfg(unix)]
#[test]
fn symlink_dependent_automatic_ownership_is_refused() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    write(
        root.join("src/Shared.pas").as_path(),
        "unit Shared; interface implementation end.",
    );
    write(
        root.join("other/Shared.pas").as_path(),
        "unit Shared; interface implementation end.",
    );
    write(root.join("A.dpr").as_path(), "program A; begin end.");
    write(root.join("B.dpr").as_path(), "program B; begin end.");
    write(
        root.join("A.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>A.dpr</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"src/Shared.pas\"/></ItemGroup></Project>",
    );
    write(
        root.join("B.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>B.dpr</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"alias/Shared.pas\"/></ItemGroup></Project>",
    );
    std::os::unix::fs::symlink(root.join("other"), root.join("alias"))
        .expect("create source alias");

    let before = discover(&root.join("src/Shared.pas"), root, &options());
    assert!(
        before.project_file.is_none() && !before.discovery_complete,
        "symlink-dependent candidate was used for automatic ownership: {before:?}"
    );

    fs::remove_file(root.join("alias")).expect("remove source alias");
    std::os::unix::fs::symlink(root.join("src"), root.join("alias"))
        .expect("retarget source alias");
    let after = discover(&root.join("src/Shared.pas"), root, &options());
    assert!(
        after.project_file.is_none() && !after.discovery_complete,
        "retargeted symlink was used for automatic ownership: {after:?}"
    );
}

#[test]
fn unknown_option_set_does_not_erase_the_known_main_source() {
    let temp = tempfile::tempdir().expect("temporary fixture");
    let root = temp.path();
    write(root.join("App.dpr").as_path(), "program App; begin end.");
    write(
        root.join("unknown.optset").as_path(),
        "<Project><PropertyGroup><MainSource>Alternative.dpr</MainSource></PropertyGroup></Project>",
    );
    write(
        root.join("App.dproj").as_path(),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup><Import Project=\"unknown.optset\" Condition=\"Exists('$(Unavailable)')\"/><PropertyGroup><MainSource>$(MainSource)</MainSource><Derived>$(MainSource)</Derived></PropertyGroup><PropertyGroup Condition=\"'$(MainSource)'=='App.dpr'\"><DCC_Define>GUESSED_MAIN</DCC_Define></PropertyGroup><PropertyGroup Condition=\"'$(Derived)'=='App.dpr'\"><DCC_Define>GUESSED_DERIVED</DCC_Define></PropertyGroup><PropertyGroup><MainSource>App.dpr</MainSource><Derived>App.dpr</Derived></PropertyGroup><PropertyGroup Condition=\"'$(MainSource)'=='App.dpr' And '$(Derived)'=='App.dpr'\"><DCC_Define>KNOWN;$(DCC_Define)</DCC_Define></PropertyGroup></Project>",
    );

    let context = discover(&root.join("App.dpr"), root, &options());

    assert_eq!(context.main_source, Some(root.join("App.dpr")));
    assert!(
        context.defines == ["KNOWN"],
        "preserved entrypoint or derived taint leaked into an uncertain condition, or reassignment did not restore certainty: {context:?}"
    );
    assert!(!context.discovery_complete);
}

#[cfg(unix)]
#[test]
fn mapped_exists_condition_revalidates_native_destination() {
    let root = tempfile::tempdir().unwrap();
    let sdk = tempfile::tempdir().unwrap();
    let main = root.path().join("App.dpr");
    let flag = sdk.path().join("enabled.flag");
    write(&main, "program App; begin end.");
    write(
        &root.path().join("App.dproj"),
        r#"<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup>
<PropertyGroup Condition="Exists('C:\SDK\enabled.flag')"><DCC_Define>MAPPED_EXISTS</DCC_Define></PropertyGroup>
</Project>"#,
    );
    let session = mapped_session(root.path(), "", r"C:\SDK", sdk.path());

    let before = ProjectContext::discover_with_overrides(
        &main,
        &[root.path().to_path_buf()],
        &options(),
        &session,
    )
    .unwrap();
    assert!(
        before.defines.is_empty(),
        "missing mapped flag was true: {before:?}"
    );
    assert!(before.metadata_files.contains(&flag));
    assert!(!before.metadata_files.iter().any(|path| {
        path.file_name()
            .is_some_and(|name| name == ".delphi-tools.local.toml")
    }));

    write(&flag, "enabled");
    let after = ProjectContext::discover_with_overrides(
        &main,
        &[root.path().to_path_buf()],
        &options(),
        &session,
    )
    .unwrap();
    assert_eq!(after.defines, ["MAPPED_EXISTS"]);
}

#[cfg(unix)]
#[test]
fn mapped_optset_import_is_loaded_and_watched_at_its_destination() {
    let root = tempfile::tempdir().unwrap();
    let sdk = tempfile::tempdir().unwrap();
    let main = root.path().join("App.dpr");
    let optset = sdk.path().join("settings.optset");
    write(&main, "program App; begin end.");
    write(
        &optset,
        "<Project><PropertyGroup><DCC_Define>MAPPED_OPTSET</DCC_Define></PropertyGroup></Project>",
    );
    write(
        &root.path().join("App.dproj"),
        r#"<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup>
<Import Project="C:\SDK\settings.optset" />
</Project>"#,
    );
    let session = mapped_session(root.path(), "", r"C:\SDK", sdk.path());
    let context = ProjectContext::discover_with_overrides(
        &main,
        &[root.path().to_path_buf()],
        &options(),
        &session,
    )
    .unwrap();

    assert_eq!(context.defines, ["MAPPED_OPTSET"]);
    assert!(context.metadata_files.contains(&optset));
    assert!(!context.metadata_files.iter().any(|path| {
        path.file_name()
            .is_some_and(|name| name == ".delphi-tools.local.toml")
    }));
}

#[cfg(unix)]
#[test]
fn mapped_optset_import_rechecks_the_final_extension_before_reading() {
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("build.targets");
    let main = root.path().join("App.dpr");
    write(&main, "program App; begin end.");
    write(
        &destination,
        "<Project><PropertyGroup><DCC_Define>MUST_NOT_LOAD</DCC_Define></PropertyGroup></Project>",
    );
    write(
        &root.path().join("App.dproj"),
        r#"<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup>
<Import Project="C:\SDK\settings.optset" />
</Project>"#,
    );
    let session = mapped_session(root.path(), "", r"C:\SDK\settings.optset", &destination);
    let context = ProjectContext::discover_with_overrides(
        &main,
        &[root.path().to_path_buf()],
        &options(),
        &session,
    )
    .unwrap();

    assert!(
        context.defines.is_empty(),
        "mapped target was evaluated: {context:?}"
    );
    assert!(
        context
            .warnings
            .iter()
            .any(|warning| warning.contains("ignored non-optset project import"))
    );
    assert!(!context.metadata_files.contains(&destination));
}

#[cfg(unix)]
#[test]
fn missing_mapped_import_reports_mapping_provenance_without_watching_config() {
    let root = tempfile::tempdir().unwrap();
    let sdk = tempfile::tempdir().unwrap();
    let main = root.path().join("App.dpr");
    write(&main, "program App; begin end.");
    write(
        &root.path().join("App.dproj"),
        r#"<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup>
<Import Project="C:\SDK\missing.optset" />
</Project>"#,
    );
    let session = mapped_session(root.path(), "", r"C:\SDK", sdk.path());
    let context = ProjectContext::discover_with_overrides(
        &main,
        &[root.path().to_path_buf()],
        &options(),
        &session,
    )
    .unwrap();
    let missing = sdk.path().join("missing.optset");
    let config = root.path().join(".delphi-tools.local.toml");

    assert!(!context.discovery_complete);
    assert!(context.metadata_files.contains(&missing));
    assert!(!context.metadata_files.contains(&config));
    assert!(context.warnings.iter().any(|warning| {
        warning.contains("missing.optset")
            && warning.contains(&config.display().to_string())
            && warning.contains(&sdk.path().display().to_string())
    }));
}

#[cfg(unix)]
#[test]
fn missing_mapped_search_path_is_retained_at_its_native_destination() {
    let root = tempfile::tempdir().unwrap();
    let sdk = tempfile::tempdir().unwrap();
    let main = root.path().join("App.dpr");
    write(&main, "program App; begin end.");
    write(
        &root.path().join("App.dproj"),
        r#"<Project><PropertyGroup><MainSource>App.dpr</MainSource>
<DCC_UnitSearchPath>C:\SDK\future</DCC_UnitSearchPath>
</PropertyGroup></Project>"#,
    );
    let session = mapped_session(root.path(), "", r"C:\SDK", sdk.path());
    let context = ProjectContext::discover_with_overrides(
        &main,
        &[root.path().to_path_buf()],
        &options(),
        &session,
    )
    .unwrap();
    let future = sdk.path().join("future");
    let config = root.path().join(".delphi-tools.local.toml");

    assert!(context.search_paths.contains(&future));
    assert!(context.warnings.iter().any(|warning| {
        warning.contains("future")
            && warning.contains(&config.display().to_string())
            && warning.contains(&sdk.path().display().to_string())
    }));
}

#[cfg(unix)]
#[test]
fn macro_defined_optset_import_is_expanded_before_path_validation() {
    let root = tempfile::tempdir().unwrap();
    let sdk = tempfile::tempdir().unwrap();
    let main = root.path().join("App.dpr");
    write(&main, "program App; begin end.");
    write(
        &sdk.path().join("settings.optset"),
        "<Project><PropertyGroup><DCC_Define>MACRO_OPTSET</DCC_Define></PropertyGroup></Project>",
    );
    write(
        &root.path().join("App.dproj"),
        r#"<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup>
<Import Project="$(SettingsFile)" />
</Project>"#,
    );
    let session = mapped_session(
        root.path(),
        r#"SettingsFile = 'C:\SDK\settings.optset'"#,
        r"C:\SDK",
        sdk.path(),
    );
    let context = ProjectContext::discover_with_overrides(
        &main,
        &[root.path().to_path_buf()],
        &options(),
        &session,
    )
    .unwrap();

    assert_eq!(context.defines, ["MACRO_OPTSET"]);
}

#[cfg(unix)]
#[test]
fn mapped_target_imports_are_ignored_without_reading_the_destination() {
    let root = tempfile::tempdir().unwrap();
    let sdk = tempfile::tempdir().unwrap();
    let main = root.path().join("App.dpr");
    let target = sdk.path().join("build.targets");
    write(&main, "program App; begin end.");
    write(
        &target,
        "<Project><PropertyGroup><DCC_Define>MUST_NOT_LOAD</DCC_Define></PropertyGroup></Project>",
    );
    write(
        &root.path().join("App.dproj"),
        r#"<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup>
<Import Project="$(TargetImport)" />
</Project>"#,
    );
    let session = mapped_session(
        root.path(),
        r#"TargetImport = 'C:\SDK\build.targets'"#,
        r"C:\SDK",
        sdk.path(),
    );
    let context = ProjectContext::discover_with_overrides(
        &main,
        &[root.path().to_path_buf()],
        &options(),
        &session,
    )
    .unwrap();

    assert!(
        context.defines.is_empty(),
        "Target was evaluated: {context:?}"
    );
    assert!(
        context
            .warnings
            .iter()
            .any(|warning| warning.contains("ignored non-optset project import"))
    );
    assert!(!context.metadata_files.contains(&target));
}

#[cfg(unix)]
#[test]
fn mapped_include_paths_remain_separate_from_unit_search_paths() {
    let root = tempfile::tempdir().unwrap();
    let sdk = tempfile::tempdir().unwrap();
    let include = sdk.path().join("include");
    let units = sdk.path().join("units");
    fs::create_dir_all(&include).unwrap();
    fs::create_dir_all(&units).unwrap();
    let main = root.path().join("App.dpr");
    write(&main, "program App; begin end.");
    write(
        &root.path().join("App.dproj"),
        r#"<Project><PropertyGroup><MainSource>App.dpr</MainSource>
<DCC_UnitSearchPath>C:\SDK\units</DCC_UnitSearchPath>
<DCC_IncludePath>C:\SDK\include</DCC_IncludePath>
</PropertyGroup></Project>"#,
    );
    let session = mapped_session(root.path(), "", r"C:\SDK", sdk.path());
    let context = ProjectContext::discover_with_overrides(
        &main,
        &[root.path().to_path_buf()],
        &options(),
        &session,
    )
    .unwrap();

    assert_eq!(context.search_paths, vec![root.path().to_path_buf(), units]);
    assert_eq!(context.include_paths, vec![include]);
}

#[cfg(unix)]
#[test]
fn mapped_native_lookup_prefers_exact_case_and_rejects_ambiguous_fallback() {
    let root = tempfile::tempdir().unwrap();
    let sdk = tempfile::tempdir().unwrap();
    let exact = sdk.path().join("Provider.pas");
    let ambiguous = sdk.path().join("provider.pas");
    let main = root.path().join("App.dpr");
    write(&exact, "unit Provider; interface implementation end.");
    write(&ambiguous, "unit Provider; interface implementation end.");
    write(&main, "program App; begin end.");
    write(
        &root.path().join("App.dproj"),
        r#"<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup>
<ItemGroup><DCCReference Include="C:\SDK\Provider.pas" /><DCCReference Include="C:\SDK\PROVIDER.pas" /></ItemGroup>
</Project>"#,
    );
    let session = mapped_session(root.path(), "", r"C:\SDK", sdk.path());
    let context = ProjectContext::discover_with_overrides(
        &main,
        &[root.path().to_path_buf()],
        &options(),
        &session,
    )
    .unwrap();

    assert_eq!(context.explicit_units.get("provider"), Some(&vec![exact]));
    let config = root.path().join(".delphi-tools.local.toml");
    assert!(context.warnings.iter().any(|warning| {
        warning.contains("ambiguous case-insensitive")
            && warning.contains(r"C:\SDK\PROVIDER.pas")
            && warning.contains(&config.display().to_string())
            && warning.contains(&sdk.path().display().to_string())
    }));
    assert!(!context.explicit_units["provider"].contains(&ambiguous));
}

#[cfg(unix)]
#[test]
fn mapped_path_resolution_errors_report_original_input_and_mapping_provenance() {
    let root = tempfile::tempdir().unwrap();
    let sdk = tempfile::tempdir().unwrap();
    let main = root.path().join("App.dpr");
    let raw = r"C:\SDK\C:\escape.pas";
    write(&main, "program App; begin end.");
    write(
        &root.path().join("App.dproj"),
        r#"<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup>
<ItemGroup><DCCReference Include="C:\SDK\C:\escape.pas" /></ItemGroup>
</Project>"#,
    );
    let session = mapped_session(root.path(), "", r"C:\SDK", sdk.path());
    let context = ProjectContext::discover_with_overrides(
        &main,
        &[root.path().to_path_buf()],
        &options(),
        &session,
    )
    .unwrap();
    let config = root.path().join(".delphi-tools.local.toml");

    assert!(context.warnings.iter().any(|warning| {
        warning.contains("mapped Windows path contains")
            && warning.contains(raw)
            && warning.contains(&config.display().to_string())
            && warning.contains(&sdk.path().display().to_string())
    }));
}

#[cfg(unix)]
#[test]
fn mapped_main_source_and_dpr_membership_are_resolved() {
    let root = tempfile::tempdir().unwrap();
    let sdk = tempfile::tempdir().unwrap();
    let main = sdk.path().join("App.dpr");
    let provider = sdk.path().join("Provider.pas");
    let project = root.path().join("App.dproj");
    write(
        &main,
        "program App; uses Provider in 'C:\\SDK\\Provider.pas'; begin end.",
    );
    write(&provider, "unit Provider; interface implementation end.");
    write(
        &project,
        r#"<Project><PropertyGroup><MainSource>C:\SDK\App.dpr</MainSource></PropertyGroup></Project>"#,
    );
    let session = mapped_session(root.path(), "", r"C:\SDK", sdk.path());
    let context = ProjectContext::discover_with_overrides(
        &main,
        &[root.path().to_path_buf()],
        &ProjectOptions {
            project_file: Some(project.clone()),
            ..ProjectOptions::default()
        },
        &session,
    )
    .unwrap();

    assert_eq!(context.main_source, Some(main));
    assert_eq!(
        context.explicit_units.get("provider"),
        Some(&vec![provider])
    );
}

#[cfg(unix)]
#[test]
fn mapped_dpk_contains_membership_is_resolved() {
    let root = tempfile::tempdir().unwrap();
    let sdk = tempfile::tempdir().unwrap();
    let package = root.path().join("App.dpk");
    let provider = sdk.path().join("Provider.pas");
    write(
        &package,
        "package App; contains Provider in 'C:\\SDK\\Provider.pas'; end.",
    );
    write(&provider, "unit Provider; interface implementation end.");
    let session = mapped_session(root.path(), "", r"C:\SDK", sdk.path());
    let context = ProjectContext::discover_with_overrides(
        &package,
        &[root.path().to_path_buf()],
        &options(),
        &session,
    )
    .unwrap();

    assert_eq!(context.main_source, Some(package));
    assert_eq!(
        context.explicit_units.get("provider"),
        Some(&vec![provider])
    );
}

#[cfg(target_os = "linux")]
#[test]
fn mapped_main_source_symlink_is_not_read_during_context_discovery() {
    let root = tempfile::tempdir().unwrap();
    let sdk = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let main = outside.path().join("Main.dpr");
    write(
        &main,
        "program Main; uses Provider in 'Provider.pas'; begin end.",
    );
    write(
        &outside.path().join("Provider.pas"),
        "unit Provider; interface implementation end.",
    );
    fs::create_dir_all(sdk.path()).unwrap();
    symlink(outside.path(), sdk.path().join("escape")).unwrap();
    write(
        &root.path().join("App.dproj"),
        r#"<Project><PropertyGroup><MainSource>C:\SDK\escape\Main.dpr</MainSource></PropertyGroup></Project>"#,
    );
    let session = mapped_session(root.path(), "", r"C:\SDK", sdk.path());

    let opened = observed_open(&main, || {
        let context = ProjectContext::discover_with_overrides(
            &root.path().join("App.dproj"),
            &[root.path().to_path_buf()],
            &options(),
            &session,
        )
        .unwrap();
        assert!(context.main_source.is_some());
    });

    assert!(!opened, "forbidden mapped MainSource was opened");
}

#[cfg(target_os = "linux")]
#[test]
fn mapped_optset_symlink_is_not_read_during_context_discovery() {
    let root = tempfile::tempdir().unwrap();
    let sdk = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let optset = outside.path().join("settings.optset");
    write(
        &optset,
        "<Project><PropertyGroup><DCC_Define>MUST_NOT_LOAD</DCC_Define></PropertyGroup></Project>",
    );
    fs::create_dir_all(sdk.path()).unwrap();
    symlink(outside.path(), sdk.path().join("escape")).unwrap();
    write(&root.path().join("App.dpr"), "program App; begin end.");
    write(
        &root.path().join("App.dproj"),
        r#"<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup><Import Project="C:\SDK\escape\settings.optset" /></Project>"#,
    );
    let session = mapped_session(root.path(), "", r"C:\SDK", sdk.path());

    let opened = observed_open(&optset, || {
        let context = ProjectContext::discover_with_overrides(
            &root.path().join("App.dpr"),
            &[root.path().to_path_buf()],
            &options(),
            &session,
        )
        .unwrap();
        assert!(
            !context
                .defines
                .iter()
                .any(|define| define == "MUST_NOT_LOAD")
        );
    });

    assert!(!opened, "forbidden mapped optset was opened");
}

#[cfg(target_os = "linux")]
#[test]
fn mapped_optset_relative_import_cannot_escape_its_mapping() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let sdk = root.path().join("sdk");
    let outside = root.path().join("outside");
    let main = root.path().join("App.dpr");
    let optset = sdk.join("settings.optset");
    let escaped = outside.join("nested.optset");

    write(&main, "program App; begin end.");
    write(
        &optset,
        r#"<Project><Import Project="../outside/nested.optset" /></Project>"#,
    );
    write(
        &escaped,
        "<Project><PropertyGroup><DCC_Define>MUST_NOT_LOAD</DCC_Define></PropertyGroup></Project>",
    );
    write(
        &root.path().join("App.dproj"),
        r#"<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup>
<Import Project="C:\SDK\settings.optset" />
</Project>"#,
    );
    let session = mapped_session(root.path(), "", r"C:\SDK", &sdk);

    let opened = observed_open(&escaped, || {
        let context = ProjectContext::discover_with_overrides(
            &main,
            &[root.path().to_path_buf()],
            &options(),
            &session,
        )
        .expect("discover project");
        assert!(
            !context.discovery_complete
                && !context
                    .defines
                    .iter()
                    .any(|define| define == "MUST_NOT_LOAD"),
            "escaped nested optset received mapped authority: {context:?}"
        );
    });

    assert!(!opened, "escaped nested optset was opened");
}

#[cfg(unix)]
#[test]
fn native_configured_optset_under_effective_mapping_is_consumed() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let sdk = tempfile::tempdir().expect("mapped destination");
    let main = root.path().join("App.dpr");
    let optset = sdk.path().join("settings.optset");

    write(&main, "program App; begin end.");
    write(
        &optset,
        "<Project><PropertyGroup><DCC_Define>NATIVE_MAPPED_OPTSET</DCC_Define></PropertyGroup></Project>",
    );
    write(
        &root.path().join("App.dproj"),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup><Import Project=\"$(BDS)/settings.optset\" /></Project>",
    );
    let session = mapped_session(
        root.path(),
        &format!("BDS = '{}'", sdk.path().display()),
        r"C:\SDK",
        sdk.path(),
    );

    let context = ProjectContext::discover_with_overrides(
        &main,
        &[root.path().to_path_buf()],
        &options(),
        &session,
    )
    .expect("discover project");

    assert!(
        context.discovery_complete,
        "native mapped optset made the context incomplete: {context:?}"
    );
    assert_eq!(context.defines, ["NATIVE_MAPPED_OPTSET"]);
    assert!(context.metadata_files.contains(&optset));
}

#[cfg(target_os = "linux")]
#[test]
fn empty_configured_substitution_does_not_authorize_external_optset() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let outside = tempfile::tempdir().expect("outside metadata");
    let main = root.path().join("App.dpr");
    let optset = outside.path().join("settings.optset");

    write(&main, "program App; begin end.");
    write(
        &optset,
        "<Project><PropertyGroup><DCC_Define>MUST_NOT_LOAD</DCC_Define></PropertyGroup></Project>",
    );
    write(
        &root.path().join(".delphi-tools.local.toml"),
        "[properties]\nPrefix = ''\n",
    );
    write(
        &root.path().join("App.dproj"),
        &format!(
            "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup><Import Project=\"$(Prefix){}\" /></Project>",
            optset.display()
        ),
    );
    let session = OverrideSession::new(None);

    let opened = observed_open(&optset, || {
        let context = ProjectContext::discover_with_overrides(
            &main,
            &[root.path().to_path_buf()],
            &options(),
            &session,
        )
        .expect("discover project");
        assert!(
            !context.discovery_complete
                && !context
                    .defines
                    .iter()
                    .any(|define| define == "MUST_NOT_LOAD"),
            "empty configured substitution received legacy authority: {context:?}"
        );
    });

    assert!(
        !opened,
        "empty configured substitution opened external optset"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn multi_candidate_denied_mapped_main_source_is_not_probed() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let sdk = tempfile::tempdir().expect("mapped destination");
    let outside = tempfile::tempdir().expect("outside source");
    let target = root.path().join("Shared.pas");
    let denied_main = outside.path().join("A.dpr");

    write(&target, "unit Shared; interface implementation end.");
    write(
        &denied_main,
        "program A; uses Shared in 'Shared.pas'; begin end.",
    );
    write(&root.path().join("B.dpr"), "program B; begin end.");
    write(
        &root.path().join("A.dproj"),
        "<Project><PropertyGroup><MainSource>C:\\SDK\\escape\\A.dpr</MainSource></PropertyGroup></Project>",
    );
    write(
        &root.path().join("B.dproj"),
        "<Project><PropertyGroup><MainSource>B.dpr</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"Shared.pas\"/></ItemGroup></Project>",
    );
    fs::create_dir_all(sdk.path()).expect("mapped destination directory");
    symlink(outside.path(), sdk.path().join("escape")).expect("mapped source escape");
    let session = mapped_session(root.path(), "", r"C:\SDK", sdk.path());

    let opened = observed_open(&denied_main, || {
        let context = ProjectContext::discover_with_overrides(
            &target,
            &[root.path().to_path_buf()],
            &options(),
            &session,
        )
        .expect("discover project candidates");
        assert!(
            context.project_file.is_none() && !context.discovery_complete,
            "denied MainSource allowed automatic ownership selection: {context:?}"
        );
    });

    assert!(
        !opened,
        "denied mapped MainSource was opened by ownership probing"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn mapped_recursive_membership_escape_is_not_read_during_ownership_probe() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let sdk = tempfile::tempdir().expect("mapped destination");
    let outside = tempfile::tempdir().expect("outside source");
    let target = root.path().join("Shared.pas");
    let denied = outside.path().join("Shared.pas");

    write(&target, "unit Shared; interface implementation end.");
    write(&denied, "unit Shared; interface implementation end.");
    write(
        &sdk.path().join("App.dpr"),
        "program App; uses Shared in 'C:\\SDK\\escape\\Shared.pas'; begin end.",
    );
    write(&root.path().join("B.dpr"), "program B; begin end.");
    write(
        &root.path().join("A.dproj"),
        r#"<Project><PropertyGroup><MainSource>C:\SDK\App.dpr</MainSource></PropertyGroup></Project>"#,
    );
    write(
        &root.path().join("B.dproj"),
        r#"<Project><PropertyGroup><MainSource>B.dpr</MainSource></PropertyGroup></Project>"#,
    );
    fs::create_dir_all(sdk.path()).expect("mapped destination");
    symlink(outside.path(), sdk.path().join("escape")).expect("mapped source escape");
    let session = mapped_session(root.path(), "", r"C:\SDK", sdk.path());

    let opened = observed_open(&denied, || {
        let context = ProjectContext::discover_with_overrides(
            &target,
            &[root.path().to_path_buf()],
            &options(),
            &session,
        )
        .expect("discover project candidates");
        assert!(
            context.project_file.is_none() && !context.discovery_complete,
            "denied recursive membership established an owner: {context:?}"
        );
    });

    assert!(!opened, "denied recursive membership was opened");
}

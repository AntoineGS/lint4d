use lsp_types::{Position, Url};
use pascal_lsp::project::{ProjectContext, ProjectOptions};
use pascal_lsp::workspace::{FileChange, Workspace, WorkspaceOptions};
use std::fs;
use std::path::{Path, PathBuf};

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create fixture directory");
    }
    fs::write(path, contents).expect("write fixture file");
}

fn options() -> ProjectOptions {
    ProjectOptions::default()
}

fn discover(file: &Path, root: &Path, options: &ProjectOptions) -> ProjectContext {
    ProjectContext::discover(file, &[root.to_path_buf()], options).expect("discover project")
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

    let context =
        ProjectContext::discover(&root.join("ws/src/U.pas"), &[root.join("ws")], &options())
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

    let context = ProjectContext::discover(
        &root.join("WS/U.pas"),
        &[root.join("ws"), root.join("WS")],
        &ProjectOptions {
            project_file: Some(PathBuf::from("App.dproj")),
            ..ProjectOptions::default()
        },
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

    let context = ProjectContext::discover(
        &root.join("App.dpr"),
        &[root.to_path_buf()],
        &ProjectOptions {
            build_config: Some("Debug".to_string()),
            platform: Some("Win64".to_string()),
            ..ProjectOptions::default()
        },
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
    let context = ProjectContext::discover(
        &root.join("Projects/Tools/WebQueryExporter/Config.pas"),
        std::slice::from_ref(&root),
        &ProjectOptions::default(),
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

use pascal_lsp::project::{ProjectContext, ProjectOptions};
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

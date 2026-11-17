use pascal_project::delphi_overrides::OverrideSession;
use pascal_project::{ProjectContext, ProjectOptions, PropertyValue};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tempfile::tempdir;

const CONFIGURED_DPROJ: &str = r#"<Project>
  <PropertyGroup>
    <Config Condition="'$(Config)'==''">Release</Config>
    <MainSource>App.pas</MainSource>
  </PropertyGroup>
  <PropertyGroup Condition="'$(Config)'=='Debug' or '$(Cfg_Debug)'!=''">
    <Cfg_Debug>true</Cfg_Debug>
    <CfgParent>Base</CfgParent>
    <Base>true</Base>
  </PropertyGroup>
  <PropertyGroup Condition="'$(Config)'=='Release' or '$(Cfg_Release)'!=''">
    <Cfg_Release>true</Cfg_Release>
    <CfgParent>Base</CfgParent>
    <Base>true</Base>
  </PropertyGroup>
  <PropertyGroup Condition="'$(Config)'=='Base' or '$(Base)'!=''">
    <Base>true</Base>
    <DCC_Hints>false</DCC_Hints>
  </PropertyGroup>
  <PropertyGroup Condition="'$(Cfg_Debug)'!=''">
    <DCC_SYMBOL_PLATFORM>false</DCC_SYMBOL_PLATFORM>
  </PropertyGroup>
  <ItemGroup>
    <BuildConfiguration Include="Base"><Key>Base</Key></BuildConfiguration>
    <BuildConfiguration Include="Debug"><Key>Cfg_Debug</Key><CfgParent>Base</CfgParent></BuildConfiguration>
    <BuildConfiguration Include="Release"><Key>Cfg_Release</Key><CfgParent>Base</CfgParent></BuildConfiguration>
  </ItemGroup>
</Project>"#;

fn write(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write test fixture");
}

fn discover(root: &Path, config: &str, dproj: &str) -> Arc<ProjectContext> {
    write(&root.join("App.pas"), "program App;\nbegin\nend.\n");
    let project = root.join("App.dproj");
    write(&project, dproj);
    ProjectContext::discover_with_overrides(
        &root.join("App.pas"),
        &[root.to_path_buf()],
        &ProjectOptions {
            project_file: Some(project),
            build_config: Some(config.to_owned()),
            ..ProjectOptions::default()
        },
        &OverrideSession::new(None),
    )
    .expect("project discovery")
    .context
}

#[test]
fn warning_properties_follow_selected_configuration_and_base() {
    let directory = tempdir().expect("temporary directory");
    let root = directory.path();

    let debug = discover(root, "Debug", CONFIGURED_DPROJ);
    assert_eq!(debug.config(), Some("Debug"));
    assert_eq!(
        debug.warning_properties().get("DCC_SYMBOL_PLATFORM"),
        Some(&PropertyValue::Known("false".to_owned()))
    );
    assert_eq!(
        debug.warning_properties().get("DCC_Hints"),
        Some(&PropertyValue::Known("false".to_owned()))
    );

    let release = discover(root, "Release", CONFIGURED_DPROJ);
    assert_eq!(release.config(), Some("Release"));
    assert!(
        !release
            .warning_properties()
            .contains_key("DCC_SYMBOL_PLATFORM")
    );
    assert_eq!(
        release.warning_properties().get("DCC_Hints"),
        Some(&PropertyValue::Known("false".to_owned()))
    );
}

#[test]
fn warning_property_with_unevaluated_expansion_is_unknown() {
    let directory = tempdir().expect("temporary directory");
    let root = directory.path();
    let dproj = r#"<Project><PropertyGroup>
      <MainSource>App.pas</MainSource>
      <DCC_SYMBOL_PLATFORM>$(Something)</DCC_SYMBOL_PLATFORM>
    </PropertyGroup></Project>"#;

    let context = discover(root, "Debug", dproj);

    assert_eq!(
        context.warning_properties().get("DCC_SYMBOL_PLATFORM"),
        Some(&PropertyValue::Unknown)
    );
}

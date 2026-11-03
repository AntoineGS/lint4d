//! MSBuild expands an undefined property to the empty string; only properties
//! the build environment always provides stay unknown (l4d-xsvq).

use pascal_project::delphi_overrides::OverrideSession;
use pascal_project::{
    CompilerVersion, ConditionalContext, ConditionalFact, DiscoveredProject, ProjectContext,
    ProjectOptions,
};
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn dproj(define: &str, extra: &str) -> String {
    dproj_with("", define, extra)
}

fn dproj_with(before: &str, define: &str, extra: &str) -> String {
    format!(
        r#"<Project xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
	<PropertyGroup>
		<ProjectVersion>12.0</ProjectVersion>
		<MainSource>App.dpr</MainSource>
		<Config Condition="'$(Config)'==''">Release</Config>
		<DCC_DCCCompiler>DCC32</DCC_DCCCompiler>
	</PropertyGroup>
	{before}
	<PropertyGroup Condition="'$(Config)'=='Base' or '$(Base)'!=''"><Base>true</Base></PropertyGroup>
	<PropertyGroup Condition="'$(Config)'=='Release' or '$(Cfg_1)'!=''"><Cfg_1>true</Cfg_1><CfgParent>Base</CfgParent><Base>true</Base></PropertyGroup>
	<PropertyGroup Condition="'$(Base)'!=''"><DCC_Define>{define}</DCC_Define><DCC_Platform>x86</DCC_Platform></PropertyGroup>
	{extra}
	<ItemGroup>
		<DelphiCompile Include="App.dpr"><MainSource>MainSource</MainSource></DelphiCompile>
		<DCCReference Include="SvcMain.pas"/>
		<BuildConfiguration Include="Base"><Key>Base</Key></BuildConfiguration>
		<BuildConfiguration Include="Release"><Key>Cfg_1</Key><CfgParent>Base</CfgParent></BuildConfiguration>
	</ItemGroup>
</Project>"#
    )
}

fn discover(root: &Path, dproj: &str) -> DiscoveredProject {
    fs::write(
        root.join("App.dpr"),
        "program App; uses SvcMain in 'SvcMain.pas'; begin end.",
    )
    .expect("main source");
    fs::write(
        root.join("SvcMain.pas"),
        "unit SvcMain; interface implementation end.",
    )
    .expect("unit");
    fs::write(root.join("App.dproj"), dproj).expect("project");
    let options = ProjectOptions {
        conditional_context: ConditionalContext::default()
            .with_compiler_version(CompilerVersion::new(21, 0)),
        ..ProjectOptions::default()
    };
    ProjectContext::discover_with_overrides(
        &root.join("App.dpr"),
        &[root.to_path_buf()],
        &options,
        &OverrideSession::new(None),
    )
    .expect("discover project")
}

#[test]
fn undefined_property_in_defines_expands_to_empty_with_a_warning() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    let discovered = discover(root, &dproj("A;$(EXTRADEFINES);$(DCC_Define)", ""));
    let context = &discovered.context;

    assert!(context.discovery_complete(), "{discovered:?}");
    assert!(context.conditional_closure().closed, "{discovered:?}");
    assert!(context.defines().iter().any(|define| define == "A"));
    let conditionals = context.conditional_context_for(&root.join("SvcMain.pas"));
    assert_eq!(conditionals.define("A"), ConditionalFact::True);
    assert_eq!(conditionals.define("CI"), ConditionalFact::False);
    assert!(
        discovered
            .observations
            .warnings
            .iter()
            .any(|warning| { warning.contains("$(EXTRADEFINES)") && warning.contains("empty") }),
        "the warning must name the property and say it was taken as empty: {:?}",
        discovered.observations.warnings
    );
}

#[test]
fn undefined_property_in_a_condition_compares_as_empty() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    let discovered = discover(
        root,
        &dproj(
            "A;$(DCC_Define)",
            r#"<PropertyGroup Condition="'$(EXTRADEFINES)'!=''"><DCC_Define>EXTRA;$(DCC_Define)</DCC_Define></PropertyGroup>
	<PropertyGroup Condition="'$(EXTRADEFINES)'==''"><DCC_Define>NOEXTRA;$(DCC_Define)</DCC_Define></PropertyGroup>"#,
        ),
    );
    let context = &discovered.context;

    assert!(context.discovery_complete(), "{discovered:?}");
    assert!(context.conditional_closure().closed, "{discovered:?}");
    let defines = context.defines();
    assert!(
        defines.iter().any(|define| define == "NOEXTRA"),
        "{defines:?}"
    );
    assert!(
        !defines.iter().any(|define| define == "EXTRA"),
        "{defines:?}"
    );
    assert!(
        discovered
            .observations
            .warnings
            .iter()
            .any(|warning| { warning.contains("$(EXTRADEFINES)") && warning.contains("empty") }),
        "{:?}",
        discovered.observations.warnings
    );
}

#[test]
fn environment_provided_property_stays_unknown() {
    for reserved in ["BDSLIB", "MSBuildProjectDirectory", "USERPROFILE"] {
        let temp = tempdir().expect("temporary directory");
        let root = temp.path();
        let discovered = discover(root, &dproj(&format!("A;$({reserved});$(DCC_Define)"), ""));
        let context = &discovered.context;

        assert!(!context.discovery_complete(), "{reserved}: {discovered:?}");
        assert!(!context.conditional_closure().closed, "{reserved}");
        let conditionals = context.conditional_context_for(&root.join("SvcMain.pas"));
        assert_eq!(
            conditionals.define("CI"),
            ConditionalFact::Unknown,
            "{reserved}"
        );
        assert!(
            !discovered
                .observations
                .warnings
                .iter()
                .any(|warning| warning.contains("empty") && warning.contains(reserved)),
            "{reserved} must not be taken as empty: {:?}",
            discovered.observations.warnings
        );
    }
}

fn taken_as_empty(discovered: &DiscoveredProject, name: &str) -> usize {
    discovered
        .observations
        .warnings
        .iter()
        .filter(|warning| warning.contains(&format!("$({name})")) && warning.contains("empty"))
        .count()
}

fn assert_unknown(root: &Path, discovered: &DiscoveredProject, name: &str) {
    let context = &discovered.context;
    assert!(!context.discovery_complete(), "{discovered:?}");
    assert!(!context.conditional_closure().closed, "{discovered:?}");
    let conditionals = context.conditional_context_for(&root.join("SvcMain.pas"));
    assert_eq!(conditionals.define("CI"), ConditionalFact::Unknown);
    assert_eq!(
        taken_as_empty(discovered, name),
        0,
        "{name} must not be taken as empty: {:?}",
        discovered.observations.warnings
    );
}

#[test]
fn undefined_property_after_an_unknown_import_stays_unknown() {
    let missing = r#"<Import Project="missing.optset" />"#;
    let temp = tempdir().expect("temporary directory");
    let discovered = discover(
        temp.path(),
        &dproj_with(missing, "A;$(EXTRADEFINES);$(DCC_Define)", ""),
    );
    assert_unknown(temp.path(), &discovered, "EXTRADEFINES");

    let temp = tempdir().expect("temporary directory");
    let discovered = discover(
        temp.path(),
        &dproj_with(
            missing,
            "A;$(DCC_Define)",
            r#"<PropertyGroup Condition="'$(EXTRADEFINES)'==''"><DCC_Define>NOEXTRA;$(DCC_Define)</DCC_Define></PropertyGroup>"#,
        ),
    );
    assert_unknown(temp.path(), &discovered, "EXTRADEFINES");
}

#[test]
fn undefined_property_after_an_ignored_targets_import_stays_unknown() {
    let targets = r#"<Import Project="Common.targets" />"#;
    let temp = tempdir().expect("temporary directory");
    let discovered = discover(
        temp.path(),
        &dproj_with(targets, "A;$(CommonDefines);$(DCC_Define)", ""),
    );
    assert_unknown(temp.path(), &discovered, "CommonDefines");

    let temp = tempdir().expect("temporary directory");
    let discovered = discover(
        temp.path(),
        &dproj_with(
            targets,
            "A;$(DCC_Define)",
            r#"<PropertyGroup Condition="'$(CommonDefines)'==''"><DCC_Define>NOCOMMON;$(DCC_Define)</DCC_Define></PropertyGroup>"#,
        ),
    );
    assert_unknown(temp.path(), &discovered, "CommonDefines");
}

#[test]
fn ignored_targets_import_keeps_values_known_before_it() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    let discovered = discover(
        root,
        &dproj(
            "A;$(EXTRADEFINES);$(DCC_Define)",
            r#"<Import Project="Common.targets" />"#,
        ),
    );
    let context = &discovered.context;
    assert!(context.discovery_complete(), "{discovered:?}");
    let conditionals = context.conditional_context_for(&root.join("SvcMain.pas"));
    assert_eq!(conditionals.define("A"), ConditionalFact::True);
    assert_eq!(conditionals.define("CI"), ConditionalFact::False);
}

/// The standard Embarcadero targets only add build steps and the IDE's own
/// properties; they never define a project's user properties.
#[test]
fn standard_embarcadero_targets_imports_do_not_define_user_properties() {
    for import in [
        r#"<Import Project="$(BDS)\Bin\CodeGear.Delphi.Targets" />"#,
        r#"<Import Project="$(BDS)\Bin\CodeGear.Common.Targets" />"#,
        r#"<Import Project="$(BDS)\Bin\CodeGear.Cpp.Targets" />"#,
        r#"<Import Project="$(APPDATA)\Embarcadero\$(BDSAPPDATABASEDIR)\$(PRODUCTVERSION)\UserTools.proj" />"#,
    ] {
        let temp = tempdir().expect("temporary directory");
        let root = temp.path();
        let discovered = discover(
            root,
            &dproj_with(import, "A;$(EXTRADEFINES);$(DCC_Define)", ""),
        );
        let context = &discovered.context;
        assert!(context.discovery_complete(), "{import}: {discovered:?}");
        let conditionals = context.conditional_context_for(&root.join("SvcMain.pas"));
        assert_eq!(
            conditionals.define("CI"),
            ConditionalFact::False,
            "{import}"
        );
    }
}

#[test]
fn undefined_property_in_an_item_condition_stays_unknown() {
    let temp = tempdir().expect("temporary directory");
    let discovered = discover(
        temp.path(),
        &dproj(
            "A;$(DCC_Define)",
            r#"<ItemGroup><DCCReference Include="Extra.pas" Condition="'$(WITHEXTRA)'=='1'"/></ItemGroup>"#,
        ),
    );
    assert!(!discovered.context.discovery_complete(), "{discovered:?}");
    assert_eq!(taken_as_empty(&discovered, "WITHEXTRA"), 0);
}

#[test]
fn undefined_property_warning_is_reported_once_and_names_the_setting() {
    let temp = tempdir().expect("temporary directory");
    let discovered = discover(
        temp.path(),
        &dproj(
            "A;$(EXTRADEFINES);$(DCC_Define)",
            r#"<PropertyGroup Condition="'$(EXTRADEFINES)'==''"><Other>$(EXTRADEFINES)</Other></PropertyGroup>"#,
        ),
    );
    assert_eq!(
        taken_as_empty(&discovered, "EXTRADEFINES"),
        1,
        "{:?}",
        discovered.observations.warnings
    );
    assert!(
        discovered
            .observations
            .warnings
            .iter()
            .any(|warning| warning.contains("$(EXTRADEFINES)") && warning.contains("[properties]")),
        "{:?}",
        discovered.observations.warnings
    );
}

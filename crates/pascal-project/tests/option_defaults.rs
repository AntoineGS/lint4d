//! D45: a switch the project, its option sets, its configurations and the
//! installation leave unset takes dcc's default for the selected installation
//! and platform; only a switch whose value cannot be determined stays unknown.

use std::{fs, path::Path};

use pascal_project::delphi_overrides::OverrideSession;
use pascal_project::{
    CompilerVersion, ConditionalContext, ConditionalFact as F, InstallationSelection,
    ProjectContext, ProjectOptions, SourceOrigin,
};

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// The `dcc32 --no-config --help` defaults of Delphi 2010 (21.0), XE3 (24.0)
/// and the Delphi 13 (37.0) dcc64, for the switches `IFOPT` evaluates.
const DCC_DEFAULTS: &[(&str, F)] = &[
    ("B", F::False),
    ("C", F::True),
    ("D", F::True),
    ("H", F::True),
    ("I", F::True),
    ("O", F::True),
    ("Q", F::False),
    ("R", F::False),
    ("T", F::False),
    ("X", F::True),
    ("Y", F::True),
];

struct Workspace {
    _temp: tempfile::TempDir,
    root: std::path::PathBuf,
}

impl Workspace {
    /// A Delphi 2010-shaped project (Base/Debug configurations and the
    /// standard targets import) selecting installation 7.0 for Win32.
    /// `groups` is inserted after the configuration groups.
    fn new(groups: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        write(&root.join("project/App.dpr"), "program App; begin end.");
        write(
            &root.join("library/External.pas"),
            "unit External; interface implementation end.",
        );
        write(
            &root.join("project/App.dproj"),
            &format!(
                r#"<Project>
  <PropertyGroup>
    <ProjectVersion>12.0</ProjectVersion>
    <MainSource>App.dpr</MainSource>
    <Config Condition="'$(Config)'==''">Debug</Config>
    <DCC_DCCCompiler>DCC32</DCC_DCCCompiler>
  </PropertyGroup>
  <PropertyGroup Condition="'$(Config)'=='Base' or '$(Base)'!=''">
    <Base>true</Base>
  </PropertyGroup>
  <PropertyGroup Condition="'$(Config)'=='Debug' or '$(Cfg_1)'!=''">
    <Cfg_1>true</Cfg_1>
    <CfgParent>Base</CfgParent>
    <Base>true</Base>
  </PropertyGroup>
  <PropertyGroup Condition="'$(Base)'!=''">
    <DCC_Define>APP;$(DCC_Define)</DCC_Define>
  </PropertyGroup>
  <PropertyGroup Condition="'$(Cfg_1)'!=''">
    <DCC_Define>DEBUG;$(DCC_Define)</DCC_Define>
  </PropertyGroup>
  {groups}
  <Import Project="$(BDS)\Bin\CodeGear.Delphi.Targets" Condition="Exists('$(BDS)\Bin\CodeGear.Delphi.Targets')"/>
</Project>"#
            ),
        );
        write(&root.join("sdk/bin/CodeGear.Delphi.Targets"), "<Project/>");
        write(
            &root.join(".delphi-tools.local.toml"),
            &format!(
                "[installations.\"7.0\".properties]\nBDS = '{}'\nPlatform = 'Win32'\n\
                 [projects.'project/App.dproj']\ninstallation = '7.0'\n",
                root.join("sdk").display()
            ),
        );
        Self { _temp: temp, root }
    }

    fn context(&self) -> ProjectContext {
        let context = ProjectContext::discover_with_overrides(
            &self.root.join("project/App.dpr"),
            std::slice::from_ref(&self.root),
            &ProjectOptions {
                project_file: Some(self.root.join("project/App.dproj")),
                ..ProjectOptions::default()
            },
            &OverrideSession::new(None),
        )
        .unwrap()
        .context;
        assert!(
            matches!(
                context.installation_selection(),
                Some(InstallationSelection::Selected { id, .. }) if id == "7.0"
            ),
            "{:?}",
            context.installation_selection()
        );
        std::sync::Arc::unwrap_or_clone(context)
    }

    fn library(&self) -> std::path::PathBuf {
        self.root.join("library/External.pas")
    }
}

fn assert_defaults(context: &ConditionalContext, which: &str) {
    for (option, expected) in DCC_DEFAULTS {
        assert_eq!(
            context.option(option),
            *expected,
            "{which} context, ${option}: {:?}",
            context.options
        );
    }
}

#[test]
fn unset_switches_take_the_dcc_defaults_of_the_selected_installation() {
    let workspace = Workspace::new("");
    let context = workspace.context();
    assert_eq!(
        context.source_origin(&workspace.library()),
        SourceOrigin::Library
    );
    assert_defaults(context.conditional_context(), "project");
    assert_defaults(context.library_conditional_context(), "library");
    assert_defaults(
        &context.conditional_context_for(&workspace.library()),
        "library source",
    );
}

#[test]
fn compiler_version_is_known_in_both_contexts_for_a_configured_installation() {
    let workspace = Workspace::new("");
    let context = workspace.context();
    assert_eq!(
        context.conditional_context().compiler_version,
        Some(CompilerVersion::new(21, 0))
    );
    assert_eq!(
        context.library_conditional_context().compiler_version,
        Some(CompilerVersion::new(21, 0))
    );
    assert!(context.library_conditional_context().rtl_constants_known);
}

#[test]
fn a_project_setting_wins_over_the_default_in_project_code_only() {
    let workspace = Workspace::new(
        "<PropertyGroup Condition=\"'$(Base)'!=''\">\
           <DCC_RangeChecking>true</DCC_RangeChecking>\
           <DCC_IntegerOverflowCheck>true</DCC_IntegerOverflowCheck>\
         </PropertyGroup>\
         <PropertyGroup Condition=\"'$(Cfg_1)'!=''\">\
           <DCC_Optimize>false</DCC_Optimize>\
           <DCC_AssertionsAtRuntime>false</DCC_AssertionsAtRuntime>\
         </PropertyGroup>",
    );
    let context = workspace.context();
    let project = context.conditional_context();
    assert_eq!(project.option("R"), F::True);
    assert_eq!(project.option("Q"), F::True);
    assert_eq!(project.option("O"), F::False);
    assert_eq!(project.option("C"), F::False);
    assert_eq!(project.option("I"), F::True);
    // Library code is compiled with the compiler's defaults, not the
    // project's switches (as it is with the project's defines).
    assert_defaults(context.library_conditional_context(), "library");
}

#[test]
fn an_option_set_setting_wins_over_the_default() {
    let workspace = Workspace::new("<Import Project=\"Checks.optset\"/>");
    write(
        &workspace.root.join("project/Checks.optset"),
        "<Project><PropertyGroup><DCC_RangeChecking>true</DCC_RangeChecking></PropertyGroup></Project>",
    );
    let context = workspace.context();
    assert_eq!(context.conditional_context().option("R"), F::True);
    assert_eq!(context.conditional_context().option("Q"), F::False);
}

#[test]
fn an_unknown_import_leaves_the_switches_it_could_set_unknown() {
    let workspace = Workspace::new("<Import Project=\"Missing.optset\"/>");
    let context = workspace.context();
    for (option, _) in DCC_DEFAULTS {
        assert_eq!(
            context.conditional_context().option(option),
            F::Unknown,
            "${option}"
        );
    }
    // The project's own metadata does not reach library code.
    assert_defaults(context.library_conditional_context(), "library");
}

#[test]
fn an_unknown_import_after_a_setting_makes_it_unknown_and_a_later_setting_wins() {
    let after = Workspace::new(
        "<PropertyGroup><DCC_RangeChecking>true</DCC_RangeChecking></PropertyGroup>\
         <Import Project=\"Missing.optset\"/>",
    );
    assert_eq!(
        after.context().conditional_context().option("R"),
        F::Unknown
    );

    let before = Workspace::new(
        "<Import Project=\"Missing.optset\"/>\
         <PropertyGroup><DCC_RangeChecking>true</DCC_RangeChecking></PropertyGroup>",
    );
    let context = before.context();
    assert_eq!(context.conditional_context().option("R"), F::True);
    assert_eq!(context.conditional_context().option("Q"), F::Unknown);
}

#[test]
fn an_unresolved_or_unknown_conditioned_setting_stays_unknown() {
    let unresolved = Workspace::new(
        "<PropertyGroup><DCC_RangeChecking>$(Missing)x</DCC_RangeChecking></PropertyGroup>",
    );
    assert_eq!(
        unresolved.context().conditional_context().option("R"),
        F::Unknown
    );
    let malformed = Workspace::new(
        "<PropertyGroup><DCC_RangeChecking>maybe</DCC_RangeChecking></PropertyGroup>",
    );
    assert_eq!(
        malformed.context().conditional_context().option("R"),
        F::Unknown
    );
}

#[test]
fn an_unknown_platform_or_compiler_keeps_unset_switches_unknown() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(&root.join("App.dpr"), "program App; begin end.");
    write(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource><Config>Debug</Config><Platform>Win32</Platform></PropertyGroup></Project>",
    );
    let context = ProjectContext::discover_with_overrides(
        &root.join("App.dpr"),
        &[root.to_path_buf()],
        &ProjectOptions::default(),
        &OverrideSession::new(None),
    )
    .unwrap()
    .context;
    assert_eq!(context.conditional_context().compiler_version, None);
    assert_eq!(context.conditional_context().option("R"), F::Unknown);
    assert_eq!(
        context.library_conditional_context().option("R"),
        F::Unknown
    );

    write(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource><Config>Debug</Config><Platform>Linux64</Platform><CompilerVersion>37.0</CompilerVersion></PropertyGroup></Project>",
    );
    let context = ProjectContext::discover_with_overrides(
        &root.join("App.dpr"),
        &[root.to_path_buf()],
        &ProjectOptions::default(),
        &OverrideSession::new(None),
    )
    .unwrap()
    .context;
    assert_eq!(context.conditional_context().option("R"), F::Unknown);
    assert_eq!(
        context.library_conditional_context().option("R"),
        F::Unknown
    );
}

#[test]
fn explicit_caller_options_win_over_the_defaults() {
    let workspace = Workspace::new("");
    let context = ProjectContext::discover_with_overrides(
        &workspace.root.join("project/App.dpr"),
        std::slice::from_ref(&workspace.root),
        &ProjectOptions {
            project_file: Some(workspace.root.join("project/App.dproj")),
            conditional_context: ConditionalContext::default().with_option("R", F::True),
            ..ProjectOptions::default()
        },
        &OverrideSession::new(None),
    )
    .unwrap()
    .context;
    assert_eq!(context.conditional_context().option("R"), F::True);
    assert_eq!(context.library_conditional_context().option("R"), F::True);
    assert_eq!(context.library_conditional_context().option("Q"), F::False);
}

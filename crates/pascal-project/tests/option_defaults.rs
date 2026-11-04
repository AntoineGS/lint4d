//! D45: a switch the project, its option sets, its configurations and the
//! installation leave unset takes dcc's default for the selected installation
//! and platform; only a switch whose value cannot be determined stays unknown.
//! Library code takes a switch only where the project's value is that
//! default, because dcc compiles search-path sources with the project's
//! switches and the switches of shipped DCUs are not known.

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

/// What `{$IFOPT X+}` answers under `dcc --no-config` with no switch given,
/// observed by compiling a probe program with dcc32 21.0, 24.0 and 37.0 and
/// dcc64 24.0 and 37.0.
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

struct Setup<'a> {
    installation: &'a str,
    platform: &'a str,
    /// Project groups after the configuration groups.
    groups: &'a str,
    /// Extra lines for `.delphi-tools.local.toml`; `None` selects
    /// `installation` for the project.
    selector: Option<&'a str>,
    /// `environment.proj` of the installation's APPDATA, if any.
    environment: Option<&'a str>,
}

impl Default for Setup<'_> {
    fn default() -> Self {
        Self {
            installation: "7.0",
            platform: "Win32",
            groups: "",
            selector: None,
            environment: None,
        }
    }
}

impl Workspace {
    /// A Delphi 2010-shaped project for installation 7.0 on Win32.
    fn new(groups: &str) -> Self {
        Self::with(Setup {
            groups,
            ..Setup::default()
        })
    }

    /// A RAD Studio template-shaped project: Base, per-platform Base and
    /// per-configuration groups, the standard targets import and the
    /// platform list.
    fn with(setup: Setup<'_>) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let groups = setup.groups;
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
    <MainSource>App.dpr</MainSource>
    <Config Condition="'$(Config)'==''">Debug</Config>
    <Base>True</Base>
    <Platform Condition="'$(Platform)'==''">Win32</Platform>
  </PropertyGroup>
  <PropertyGroup Condition="'$(Config)'=='Base' or '$(Base)'!=''">
    <Base>true</Base>
  </PropertyGroup>
  <PropertyGroup Condition="('$(Platform)'=='Win32' and '$(Base)'=='true') or '$(Base_Win32)'!=''">
    <Base_Win32>true</Base_Win32>
    <CfgParent>Base</CfgParent>
    <Base>true</Base>
  </PropertyGroup>
  <PropertyGroup Condition="('$(Platform)'=='Win64' and '$(Base)'=='true') or '$(Base_Win64)'!=''">
    <Base_Win64>true</Base_Win64>
    <CfgParent>Base</CfgParent>
    <Base>true</Base>
  </PropertyGroup>
  <PropertyGroup Condition="'$(Config)'=='Debug' or '$(Cfg_1)'!=''">
    <Cfg_1>true</Cfg_1>
    <CfgParent>Base</CfgParent>
    <Base>true</Base>
  </PropertyGroup>
  <PropertyGroup Condition="('$(Platform)'=='Win64' and '$(Cfg_1)'=='true') or '$(Cfg_1_Win64)'!=''">
    <Cfg_1_Win64>true</Cfg_1_Win64>
    <CfgParent>Cfg_1</CfgParent>
    <Cfg_1>true</Cfg_1>
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
  <ProjectExtensions><BorlandProject><Platforms>
    <Platform value="Win32">True</Platform><Platform value="Win64">True</Platform>
  </Platforms></BorlandProject></ProjectExtensions>
</Project>"#
            ),
        );
        let installation = setup.installation;
        let sdk = root.join(format!("sdk/{installation}"));
        let appdata = root.join(format!("appdata/{installation}"));
        write(&sdk.join("bin/CodeGear.Delphi.Targets"), "<Project/>");
        if let Some(environment) = setup.environment {
            write(&appdata.join("environment.proj"), environment);
        }
        let selector = setup.selector.map_or_else(
            || format!("[projects.'project/App.dproj']\ninstallation = '{installation}'\n"),
            str::to_owned,
        );
        write(
            &root.join(".delphi-tools.local.toml"),
            &format!(
                "[installations.\"{installation}\".properties]\nBDS = '{}'\nAPPDATA = '{}'\nPlatform = '{}'\n{selector}",
                sdk.display(),
                appdata.display(),
                setup.platform,
            ),
        );
        Self { _temp: temp, root }
    }

    fn discover(&self, conditional_context: ConditionalContext) -> ProjectContext {
        let context = ProjectContext::discover_with_overrides(
            &self.root.join("project/App.dpr"),
            std::slice::from_ref(&self.root),
            &ProjectOptions {
                project_file: Some(self.root.join("project/App.dproj")),
                conditional_context,
                ..ProjectOptions::default()
            },
            &OverrideSession::new(None),
        )
        .unwrap()
        .context;
        std::sync::Arc::unwrap_or_clone(context)
    }

    fn context(&self) -> ProjectContext {
        let context = self.discover(ConditionalContext::default());
        assert!(
            matches!(
                context.installation_selection(),
                Some(InstallationSelection::Selected { .. })
            ),
            "{:?}",
            context.installation_selection()
        );
        context
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

fn assert_all_unknown(context: &ConditionalContext, which: &str) {
    for (option, _) in DCC_DEFAULTS {
        assert_eq!(
            context.option(option),
            F::Unknown,
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
fn a_project_setting_wins_in_project_code_and_leaves_library_code_unknown() {
    let workspace = Workspace::new(
        "<PropertyGroup Condition=\"'$(Base)'!=''\">\
           <DCC_RangeChecking>true</DCC_RangeChecking>\
           <DCC_IntegerOverflowCheck>true</DCC_IntegerOverflowCheck>\
           <DCC_IOChecking>true</DCC_IOChecking>\
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
    // Library code is compiled with the project's switches when its source
    // is on the search path, and with unknown ones when a DCU ships: only a
    // project value equal to the default gives it a definite answer.
    let library = context.library_conditional_context();
    for option in ["R", "Q", "O", "C"] {
        assert_eq!(library.option(option), F::Unknown, "${option}");
    }
    assert_eq!(library.option("I"), F::True);
    assert_eq!(library.option("B"), F::False);
}

#[test]
fn a_template_shaped_project_keeps_library_code_unknown_where_it_differs() {
    let workspace = Workspace::new(
        "<PropertyGroup Condition=\"'$(Base_Win32)'!=''\">\
           <DCC_RangeChecking>true</DCC_RangeChecking>\
         </PropertyGroup>\
         <PropertyGroup Condition=\"'$(Cfg_1)'!=''\">\
           <DCC_Optimize>False</DCC_Optimize>\
         </PropertyGroup>",
    );
    let context = workspace.context();
    assert_eq!(context.conditional_context().option("O"), F::False);
    assert_eq!(context.conditional_context().option("R"), F::True);
    assert_eq!(
        context.library_conditional_context().option("O"),
        F::Unknown
    );
    assert_eq!(
        context.library_conditional_context().option("R"),
        F::Unknown
    );
    assert_eq!(context.library_conditional_context().option("Q"), F::False);
}

#[test]
fn per_platform_groups_select_the_win64_values() {
    let workspace = Workspace::with(Setup {
        installation: "37.0",
        platform: "Win64",
        groups: "<PropertyGroup Condition=\"'$(Base_Win32)'!=''\">\
                   <DCC_RangeChecking>true</DCC_RangeChecking>\
                 </PropertyGroup>\
                 <PropertyGroup Condition=\"'$(Cfg_1_Win64)'!=''\">\
                   <DCC_IntegerOverflowCheck>true</DCC_IntegerOverflowCheck>\
                 </PropertyGroup>",
        ..Setup::default()
    });
    let context = workspace.context();
    assert_eq!(context.platform(), Some("Win64"));
    assert_eq!(
        context.conditional_context().compiler_version,
        Some(CompilerVersion::new(37, 0))
    );
    assert_eq!(context.conditional_context().option("R"), F::False);
    assert_eq!(context.conditional_context().option("Q"), F::True);
    assert_eq!(context.library_conditional_context().option("R"), F::False);
    assert_eq!(
        context.library_conditional_context().option("Q"),
        F::Unknown
    );
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
    assert_eq!(
        context.library_conditional_context().option("R"),
        F::Unknown
    );
}

#[test]
fn an_unknown_import_leaves_the_switches_it_could_set_unknown() {
    let workspace = Workspace::new("<Import Project=\"Missing.optset\"/>");
    let context = workspace.context();
    assert_all_unknown(context.conditional_context(), "project");
    assert_all_unknown(context.library_conditional_context(), "library");
}

/// An unknown import may set `GlobalOptionFile`, which CodeGear.Common.Targets
/// imports after every project group, so even a later setting is not final.
#[test]
fn an_unknown_import_leaves_even_later_settings_unknown() {
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
    assert_eq!(context.conditional_context().option("R"), F::Unknown);
    assert_eq!(context.conditional_context().option("Q"), F::Unknown);
}

#[test]
fn a_global_option_file_leaves_every_switch_unknown() {
    for value in ["Opts.proj", "$(Missing)Opts.proj"] {
        let workspace = Workspace::new(&format!(
            "<PropertyGroup>\
               <GlobalOptionFile>{value}</GlobalOptionFile>\
               <DCC_RangeChecking>true</DCC_RangeChecking>\
             </PropertyGroup>"
        ));
        let context = workspace.context();
        assert_all_unknown(context.conditional_context(), "project");
        assert_all_unknown(context.library_conditional_context(), "library");
    }
    let environment = Workspace::with(Setup {
        environment: Some(
            "<Project><PropertyGroup><GlobalOptionFile>C:\\Opts.proj</GlobalOptionFile></PropertyGroup></Project>",
        ),
        ..Setup::default()
    });
    let context = environment.context();
    assert_all_unknown(context.conditional_context(), "project");
    assert_all_unknown(context.library_conditional_context(), "library");
}

#[test]
fn an_installation_setting_applies_to_both_contexts_unless_the_project_differs() {
    let environment = Some(
        "<Project><PropertyGroup><DCC_RangeChecking>true</DCC_RangeChecking></PropertyGroup></Project>",
    );
    let workspace = Workspace::with(Setup {
        environment,
        ..Setup::default()
    });
    let context = workspace.context();
    assert_eq!(context.conditional_context().option("R"), F::True);
    assert_eq!(context.library_conditional_context().option("R"), F::True);
    assert_eq!(context.library_conditional_context().option("Q"), F::False);

    // environment.proj is imported after the project's groups, so it may
    // override a different project value.
    let differing = Workspace::with(Setup {
        environment,
        groups: "<PropertyGroup><DCC_RangeChecking>false</DCC_RangeChecking></PropertyGroup>",
        ..Setup::default()
    });
    let context = differing.context();
    assert_eq!(context.conditional_context().option("R"), F::Unknown);
    assert_eq!(
        context.library_conditional_context().option("R"),
        F::Unknown
    );
}

#[test]
fn debug_and_symbol_information_levels_follow_the_dcc_task() {
    for (raw, debug, symbols) in [
        ("0", F::False, F::False),
        ("1", F::True, F::True),
        ("2", F::True, F::True),
        ("true", F::True, F::Unknown),
        ("false", F::False, F::Unknown),
        ("3", F::Unknown, F::Unknown),
    ] {
        let workspace = Workspace::new(&format!(
            "<PropertyGroup>\
               <DCC_DebugInformation>{raw}</DCC_DebugInformation>\
               <DCC_SymbolReferenceInfo>{raw}</DCC_SymbolReferenceInfo>\
             </PropertyGroup>"
        ));
        let context = workspace.context();
        assert_eq!(context.conditional_context().option("D"), debug, "D {raw}");
        assert_eq!(
            context.conditional_context().option("Y"),
            symbols,
            "Y {raw}"
        );
    }
}

#[test]
fn boolean_switches_accept_msbuild_booleans_only() {
    for (raw, expected) in [
        ("True", F::True),
        ("on", F::True),
        ("Yes", F::True),
        ("!false", F::True),
        ("FALSE", F::False),
        ("off", F::False),
        ("1", F::Unknown),
        ("+", F::Unknown),
        ("maybe", F::Unknown),
    ] {
        let workspace = Workspace::new(&format!(
            "<PropertyGroup><DCC_RangeChecking>{raw}</DCC_RangeChecking></PropertyGroup>"
        ));
        assert_eq!(
            workspace.context().conditional_context().option("R"),
            expected,
            "{raw}"
        );
    }
}

#[test]
fn an_unresolved_setting_stays_unknown() {
    let unresolved = Workspace::new(
        "<PropertyGroup><DCC_RangeChecking>$(Missing)x</DCC_RangeChecking></PropertyGroup>",
    );
    assert_eq!(
        unresolved.context().conditional_context().option("R"),
        F::Unknown
    );
}

/// Every XE+ template has `<ProjectName Condition="'$(ProjectName)'==''">`,
/// which cannot be evaluated (ProjectName can come from outside); an
/// element that could set only other properties does not touch a switch.
#[test]
fn an_unevaluated_element_setting_other_properties_keeps_the_defaults() {
    let workspace = Workspace::new(
        "<PropertyGroup><ProjectName Condition=\"'$(ProjectName)'==''\">App</ProjectName></PropertyGroup>\
         <PropertyGroup Condition=\"'$(ProjectName)'=='Other'\"><Other>1</Other></PropertyGroup>",
    );
    let context = workspace.context();
    assert!(!context.discovery_complete());
    assert_defaults(context.conditional_context(), "project");
    assert_defaults(context.library_conditional_context(), "library");
}

/// A group or property element that cannot be evaluated could have set the
/// switch property it names, and only that one.
#[test]
fn an_unevaluated_element_setting_a_switch_keeps_only_that_switch_unknown() {
    let workspace = Workspace::new(
        "<PropertyGroup Condition=\"'$(ProjectName)'=='Other'\">\
           <DCC_RangeChecking>true</DCC_RangeChecking>\
         </PropertyGroup>\
         <PropertyGroup>\
           <DCC_IntegerOverflowCheck Condition=\"'$(Config)' &gt; 'Debug'\">true</DCC_IntegerOverflowCheck>\
           <DCC_Optimize>false</DCC_Optimize>\
         </PropertyGroup>",
    );
    let context = workspace.context();
    assert!(!context.discovery_complete());
    for which in [
        context.conditional_context(),
        context.library_conditional_context(),
    ] {
        assert_eq!(which.option("R"), F::Unknown, "{:?}", which.options);
        assert_eq!(which.option("Q"), F::Unknown, "{:?}", which.options);
        assert_eq!(which.option("I"), F::True, "{:?}", which.options);
        assert_eq!(which.option("B"), F::False, "{:?}", which.options);
    }
    assert_eq!(context.conditional_context().option("O"), F::False);
    assert_eq!(
        context.library_conditional_context().option("O"),
        F::Unknown
    );
}

fn group(properties: &str) -> String {
    format!("<PropertyGroup>{properties}</PropertyGroup>")
}

fn choose(whens: &[(&str, &str)], otherwise: Option<&str>) -> String {
    let mut text = String::from("<Choose>");
    for (condition, properties) in whens {
        text.push_str(&format!(
            "<When Condition=\"{condition}\">{}</When>",
            group(properties)
        ));
    }
    if let Some(properties) = otherwise {
        text.push_str(&format!("<Otherwise>{}</Otherwise>", group(properties)));
    }
    text.push_str("</Choose>");
    text
}

const RANGE_ON: &str = "<DCC_RangeChecking>true</DCC_RangeChecking>";
const RANGE_OFF: &str = "<DCC_RangeChecking>false</DCC_RangeChecking>";
const OVERFLOW_ON: &str = "<DCC_IntegerOverflowCheck>true</DCC_IntegerOverflowCheck>";

#[test]
fn the_first_true_when_applies_and_later_branches_are_skipped() {
    let workspace = Workspace::new(&choose(
        &[
            ("'$(Config)'=='Debug'", RANGE_ON),
            ("'$(Config)'=='Debug'", OVERFLOW_ON),
            ("'$(Config)'=='Release'", OVERFLOW_ON),
        ],
        Some(OVERFLOW_ON),
    ));
    let context = workspace.context();
    assert_eq!(context.conditional_context().option("R"), F::True);
    assert_eq!(context.conditional_context().option("Q"), F::False);
}

#[test]
fn otherwise_applies_only_when_every_when_is_false() {
    let workspace = Workspace::new(&choose(
        &[
            ("'$(Config)'=='Release'", RANGE_ON),
            ("'$(Config)'=='Other'", OVERFLOW_ON),
        ],
        Some(RANGE_OFF),
    ));
    let context = workspace.context();
    assert_eq!(context.conditional_context().option("R"), F::False);
    assert_eq!(context.conditional_context().option("Q"), F::False);
    // A true When suppresses Otherwise.
    let workspace = Workspace::new(&choose(
        &[("'$(Config)'=='Debug'", RANGE_ON)],
        Some(OVERFLOW_ON),
    ));
    let context = workspace.context();
    assert_eq!(context.conditional_context().option("R"), F::True);
    assert_eq!(context.conditional_context().option("Q"), F::False);
}

#[test]
fn an_unknown_when_leaves_every_switch_a_branch_could_set_unknown() {
    let workspace = Workspace::new(&choose(
        &[
            ("'$(ProjectName)'=='Other'", RANGE_ON),
            (
                "'$(ProjectName)'=='Y'",
                "<DCC_Optimize>false</DCC_Optimize>",
            ),
        ],
        Some(OVERFLOW_ON),
    ));
    let context = workspace.context();
    for which in [
        context.conditional_context(),
        context.library_conditional_context(),
    ] {
        for option in ["R", "O", "Q"] {
            assert_eq!(which.option(option), F::Unknown, "${option}");
        }
        assert_eq!(which.option("I"), F::True);
        assert_eq!(which.option("B"), F::False);
    }
}

#[test]
fn a_choose_cannot_clear_an_earlier_unevaluated_assignment() {
    let workspace = Workspace::new(&format!(
        "<PropertyGroup Condition=\"'$(ProjectName)'=='Other'\">{RANGE_ON}</PropertyGroup>{}",
        choose(&[("'$(Config)'=='Release'", RANGE_OFF)], Some(RANGE_OFF)),
    ));
    // Otherwise applies here, so a definite assignment does clear it...
    assert_eq!(
        workspace.context().conditional_context().option("R"),
        F::False
    );
    // ...but an unknown branch condition leaves the switch unknown.
    let workspace = Workspace::new(&format!(
        "<PropertyGroup Condition=\"'$(ProjectName)'=='Other'\">{RANGE_ON}</PropertyGroup>{}",
        choose(&[("'$(ProjectName)'=='X'", RANGE_OFF)], Some(RANGE_OFF)),
    ));
    assert_eq!(
        workspace.context().conditional_context().option("R"),
        F::Unknown
    );
}

#[test]
fn a_later_assignment_clears_an_unevaluated_one_only_when_it_applies() {
    let unknown = format!(
        "<PropertyGroup Condition=\"'$(ProjectName)'=='Other'\">{RANGE_ON}</PropertyGroup>"
    );
    for (later, expected) in [
        (group(RANGE_OFF), F::False),
        (
            format!(
                "<PropertyGroup Condition=\"'$(Config)'=='Debug'\">{RANGE_OFF}</PropertyGroup>"
            ),
            F::False,
        ),
        (
            format!(
                "<PropertyGroup Condition=\"'$(Config)'=='Release'\">{RANGE_OFF}</PropertyGroup>"
            ),
            F::Unknown,
        ),
    ] {
        let workspace = Workspace::new(&format!("{unknown}{later}"));
        assert_eq!(
            workspace.context().conditional_context().option("R"),
            expected,
            "{later}"
        );
    }
}

#[test]
fn a_value_dropped_for_the_property_budget_is_unknown_for_its_dependents() {
    // The fillers use up the 16 MiB budget (1000 KB each), so the last
    // assignment of DCC_RangeChecking is dropped; its earlier value must
    // not survive for `$(DCC_RangeChecking)` to read.
    let mut groups = format!(
        "<DCC_RangeChecking>true</DCC_RangeChecking><Seed>{}</Seed>",
        "x".repeat(500 * 1000)
    );
    for index in 0..16 {
        groups.push_str(&format!("<Filler{index}>$(Seed)$(Seed)</Filler{index}>"));
    }
    groups.push_str(
        "<DCC_RangeChecking>$(Seed)$(Seed)</DCC_RangeChecking>\
         <DCC_IntegerOverflowCheck>$(DCC_RangeChecking)</DCC_IntegerOverflowCheck>",
    );
    let workspace = Workspace::new(&format!("<PropertyGroup>{groups}</PropertyGroup>"));
    let context = workspace.context();
    assert_eq!(context.conditional_context().option("R"), F::Unknown);
    assert_eq!(context.conditional_context().option("Q"), F::Unknown);
}

#[test]
fn an_unselected_or_invalid_installation_keeps_switches_unknown() {
    let needs_choice = Workspace::with(Setup {
        selector: Some("[installations.\"10.0\".properties]\nBDS = '/sdk/10'\n"),
        ..Setup::default()
    });
    let context = needs_choice.discover(ConditionalContext::default());
    assert!(
        matches!(
            context.installation_selection(),
            Some(InstallationSelection::NeedsChoice { .. })
        ),
        "{:?}",
        context.installation_selection()
    );
    assert_all_unknown(context.conditional_context(), "project");
    assert_all_unknown(context.library_conditional_context(), "library");

    let invalid = Workspace::with(Setup {
        selector: Some("[projects.'project/App.dproj']\ninstallation = '99.0'\n"),
        ..Setup::default()
    });
    let context = invalid.discover(ConditionalContext::default());
    assert!(
        matches!(
            context.installation_selection(),
            Some(InstallationSelection::Invalid { .. })
        ),
        "{:?}",
        context.installation_selection()
    );
    assert_all_unknown(context.conditional_context(), "project");
    assert_all_unknown(context.library_conditional_context(), "library");
}

#[test]
fn an_unknown_platform_or_compiler_keeps_unset_switches_unknown() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(&root.join("App.dpr"), "program App; begin end.");
    let discover = |dproj: &str| {
        write(&root.join("App.dproj"), dproj);
        ProjectContext::discover_with_overrides(
            &root.join("App.dpr"),
            &[root.to_path_buf()],
            &ProjectOptions::default(),
            &OverrideSession::new(None),
        )
        .unwrap()
        .context
    };
    let project = |properties: &str| {
        format!(
            "<Project><PropertyGroup><MainSource>App.dpr</MainSource><Config>Debug</Config>{properties}</PropertyGroup></Project>"
        )
    };

    let context = discover(&project("<Platform>Win32</Platform>"));
    assert_eq!(context.conditional_context().compiler_version, None);
    assert_eq!(context.conditional_context().option("R"), F::Unknown);
    assert_eq!(
        context.library_conditional_context().option("R"),
        F::Unknown
    );

    let context = discover(&project(
        "<Platform>Linux64</Platform><CompilerVersion>37.0</CompilerVersion>",
    ));
    assert_eq!(context.conditional_context().option("R"), F::Unknown);
    assert_eq!(
        context.library_conditional_context().option("R"),
        F::Unknown
    );

    // dcc 36.0 (installation 23.0) was not available to probe.
    let context = discover(&project(
        "<Platform>Win32</Platform><CompilerVersion>36.0</CompilerVersion>",
    ));
    assert_eq!(context.conditional_context().option("R"), F::Unknown);
    assert_eq!(
        context.library_conditional_context().option("R"),
        F::Unknown
    );

    // dcc 21.0 has no Win64 compiler.
    let context = discover(&project(
        "<Platform>Win64</Platform><CompilerVersion>21.0</CompilerVersion>",
    ));
    assert_eq!(context.conditional_context().option("R"), F::Unknown);

    let context = discover(&project(
        "<Platform>Win64</Platform><CompilerVersion>24.0</CompilerVersion>",
    ));
    assert_eq!(context.conditional_context().option("R"), F::False);
    assert_eq!(context.library_conditional_context().option("R"), F::False);
}

#[test]
fn explicit_caller_options_win_over_the_defaults() {
    let workspace = Workspace::new("");
    let context = workspace.discover(ConditionalContext::default().with_option("R", F::True));
    assert_eq!(context.conditional_context().option("R"), F::True);
    assert_eq!(context.library_conditional_context().option("R"), F::True);
    assert_eq!(context.library_conditional_context().option("Q"), F::False);
}

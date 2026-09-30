use pascal_project::build_selection::{
    BuildChoice, BuildSelection, BuildSelectionMode, parse_build_candidates,
};
use pascal_project::delphi_overrides::OverrideSession;
use pascal_project::{ProjectContext, ProjectOptions};
use std::fs;
use std::path::Path;
use tempfile::tempdir;

const WEBQUERY_DPROJ: &str = r#"<Project xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
	<PropertyGroup>
		<ProjectVersion>12.0</ProjectVersion>
		<MainSource>App.dpr</MainSource>
		<Config Condition="'$(Config)'==''">Release</Config>
		<DCC_DCCCompiler>DCC32</DCC_DCCCompiler>
	</PropertyGroup>
	<PropertyGroup Condition="'$(Config)'=='Base' or '$(Base)'!=''"><Base>true</Base></PropertyGroup>
	<PropertyGroup Condition="'$(Config)'=='Release' or '$(Cfg_1)'!=''"><Cfg_1>true</Cfg_1><CfgParent>Base</CfgParent><Base>true</Base></PropertyGroup>
	<PropertyGroup Condition="'$(Config)'=='Debug' or '$(Cfg_2)'!=''"><Cfg_2>true</Cfg_2><CfgParent>Base</CfgParent><Base>true</Base></PropertyGroup>
	<PropertyGroup Condition="'$(Base)'!=''"><DCC_Define>NOSF;$(DCC_Define)</DCC_Define><DCC_Platform>x86</DCC_Platform></PropertyGroup>
	<PropertyGroup Condition="'$(Cfg_1)'!=''"><DCC_Define>RELEASE;$(DCC_Define)</DCC_Define></PropertyGroup>
	<PropertyGroup Condition="'$(Cfg_2)'!=''"><DCC_Define>DEBUG;$(DCC_Define)</DCC_Define></PropertyGroup>
	<ItemGroup>
		<DelphiCompile Include="App.dpr"><MainSource>MainSource</MainSource></DelphiCompile>
		<DCCReference Include="SvcMain.pas"/>
		<BuildConfiguration Include="Base"><Key>Base</Key></BuildConfiguration>
		<BuildConfiguration Include="Debug"><Key>Cfg_2</Key><CfgParent>Base</CfgParent></BuildConfiguration>
		<BuildConfiguration Include="Release"><Key>Cfg_1</Key><CfgParent>Base</CfgParent></BuildConfiguration>
	</ItemGroup>
</Project>"#;

#[test]
fn parses_webquery_configurations_platform_and_defaults_with_bom() {
    let xml = format!("\u{feff}{WEBQUERY_DPROJ}");

    let candidates = parse_build_candidates(&xml);

    assert_eq!(candidates.configs, ["Debug", "Release"]);
    assert_eq!(candidates.platforms, ["Win32"]);
    assert_eq!(candidates.default_config.as_deref(), Some("Release"));
    assert_eq!(candidates.default_platform.as_deref(), Some("Win32"));
}

#[test]
fn parses_enabled_platforms_and_keeps_custom_configurations_in_document_order() {
    let xml = r#"<Project xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
      <PropertyGroup>
        <ProjectVersion>23.0</ProjectVersion>
        <Config Condition="'$(Config)'==''">Release</Config>
        <Platform Condition="'$(Platform)'==''">Win32</Platform>
      </PropertyGroup>
      <ItemGroup>
        <BuildConfiguration Include="Debug"/>
        <BuildConfiguration Include="debug"/>
        <BuildConfiguration Include="Console"/>
        <BuildConfiguration Include="Base"/>
        <BuildConfiguration Include="base"/>
        <BuildConfiguration Include="Release"/>
      </ItemGroup>
      <ProjectExtensions><BorlandProject><Platforms>
        <Platform value="Win32">True</Platform>
        <Platform value="Win64">False</Platform>
      </Platforms></BorlandProject></ProjectExtensions>
    </Project>"#;

    let candidates = parse_build_candidates(xml);

    assert_eq!(candidates.configs, ["Debug", "Console", "Release"]);
    assert_eq!(candidates.platforms, ["Win32"]);
    assert_eq!(candidates.default_config.as_deref(), Some("Release"));
    assert_eq!(candidates.default_platform.as_deref(), Some("Win32"));
}

#[test]
fn derives_an_implicit_platform_only_when_platforms_block_is_absent() {
    for (xml, expected) in [
        (
            "<Project><PropertyGroup><ProjectVersion>12.0</ProjectVersion></PropertyGroup></Project>",
            Some("Win32"),
        ),
        (
            "<Project><PropertyGroup><DCC_DCCCompiler>DCC64</DCC_DCCCompiler><DCC_Platform>x86</DCC_Platform></PropertyGroup></Project>",
            Some("Win64"),
        ),
        (
            "<Project><PropertyGroup><DCC_DCCCompiler>Other</DCC_DCCCompiler><DCC_Platform>x64</DCC_Platform></PropertyGroup></Project>",
            Some("Win64"),
        ),
        (
            "<Project><ProjectExtensions><BorlandProject><Platforms/></BorlandProject></ProjectExtensions><PropertyGroup><DCC_DCCCompiler>DCC32</DCC_DCCCompiler></PropertyGroup></Project>",
            None,
        ),
    ] {
        assert_eq!(
            parse_build_candidates(xml)
                .platforms
                .first()
                .map(String::as_str),
            expected
        );
    }
}

#[test]
fn discovers_webquery_build_selection_with_project_defaults_and_candidate_spelling() {
    let context = discover_webquery(&ProjectOptions::default());

    assert_eq!(
        context.config_selection.selected.as_deref(),
        Some("Release")
    );
    assert_eq!(
        context.config_selection.mode,
        BuildSelectionMode::ProjectDefault
    );
    assert_eq!(context.config_selection.candidates, ["Debug", "Release"]);
    assert_eq!(
        context.config_selection.project_default.as_deref(),
        Some("Release")
    );
    assert_eq!(context.platform.as_deref(), Some("x86"));
    assert_eq!(
        context.platform_selection.selected.as_deref(),
        Some("Win32")
    );
    assert_eq!(
        context.platform_selection.mode,
        BuildSelectionMode::ProjectDefault
    );
    assert_eq!(context.platform_selection.candidates, ["Win32"]);
    assert_eq!(
        context.platform_selection.project_default.as_deref(),
        Some("Win32")
    );
}

#[test]
fn explicit_build_configuration_is_reported_using_candidate_spelling() {
    let options = ProjectOptions {
        build_config: Some("debug".into()),
        ..ProjectOptions::default()
    };

    let context = discover_webquery(&options);

    assert_eq!(context.config_selection.selected.as_deref(), Some("Debug"));
    assert_eq!(
        context.config_selection.mode,
        BuildSelectionMode::Configured
    );
}

#[test]
fn session_build_choice_selects_debug_and_overrides_project_configuration() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    write_webquery_fixture(root);
    write(
        &root.join("config.toml"),
        "[projects.\"App.dproj\"]\nconfig = 'Release'\n",
    );
    let project_file = root.join("App.dproj");
    let options = ProjectOptions {
        build_config: Some("Release".into()),
        build_selections: [(
            project_file,
            BuildChoice {
                config: Some("Debug".into()),
                ..BuildChoice::default()
            },
        )]
        .into_iter()
        .collect(),
        ..ProjectOptions::default()
    };
    let overrides = OverrideSession::new(Some(root.join("config.toml")));

    let context = discover_webquery_at(root, &options, &overrides);

    assert_eq!(context.config_selection.mode, BuildSelectionMode::Session);
    assert_eq!(context.config_selection.selected.as_deref(), Some("Debug"));
    assert!(context.defines.iter().any(|define| define == "DEBUG"));
    assert!(!context.defines.iter().any(|define| define == "RELEASE"));
}

#[test]
fn config_toml_build_choice_selects_debug_over_global_option() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    write_webquery_fixture(root);
    write(
        &root.join("config.toml"),
        "[projects.\"App.dproj\"]\nconfig = 'Debug'\nplatform = 'Win32'\n",
    );
    let options = ProjectOptions {
        build_config: Some("Release".into()),
        ..ProjectOptions::default()
    };
    let overrides = OverrideSession::new(Some(root.join("config.toml")));

    let context = discover_webquery_at(root, &options, &overrides);

    assert_eq!(
        context.config_selection.mode,
        BuildSelectionMode::Configured
    );
    assert_eq!(context.config_selection.selected.as_deref(), Some("Debug"));
    assert_eq!(
        context.platform_selection.mode,
        BuildSelectionMode::Configured
    );
    assert_eq!(
        context.platform_selection.selected.as_deref(),
        Some("Win32")
    );
    assert!(context.defines.iter().any(|define| define == "DEBUG"));
    assert!(!context.defines.iter().any(|define| define == "RELEASE"));
}

#[test]
fn config_toml_platform_choice_overrides_global_platform() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    write_webquery_fixture_with_platforms(root);
    write(
        &root.join("config.toml"),
        "[projects.\"App.dproj\"]\nplatform = 'Win64'\n",
    );
    let options = ProjectOptions {
        platform: Some("Win32".into()),
        ..ProjectOptions::default()
    };
    let overrides = OverrideSession::new(Some(root.join("config.toml")));

    let context = discover_webquery_at(root, &options, &overrides);

    assert_eq!(
        context.platform_selection.mode,
        BuildSelectionMode::Configured
    );
    assert_eq!(
        context.platform_selection.selected.as_deref(),
        Some("Win64")
    );
    assert_eq!(context.platform.as_deref(), Some("Win64"));
}

#[test]
fn session_platform_choice_overrides_config_toml_platform() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    write_webquery_fixture_with_platforms(root);
    write(
        &root.join("config.toml"),
        "[projects.\"App.dproj\"]\nplatform = 'Win64'\n",
    );
    let options = ProjectOptions {
        platform: Some("Win64".into()),
        build_selections: [(
            root.join("App.dproj"),
            BuildChoice {
                platform: Some("Win32".into()),
                ..BuildChoice::default()
            },
        )]
        .into_iter()
        .collect(),
        ..ProjectOptions::default()
    };
    let overrides = OverrideSession::new(Some(root.join("config.toml")));

    let context = discover_webquery_at(root, &options, &overrides);

    assert_eq!(context.platform_selection.mode, BuildSelectionMode::Session);
    assert_eq!(
        context.platform_selection.selected.as_deref(),
        Some("Win32")
    );
    assert_eq!(context.platform.as_deref(), Some("Win32"));
}

#[test]
fn invalid_session_build_choice_is_reported_without_falling_back() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    write_webquery_fixture(root);
    let project_file = root.join("App.dproj");
    let options = ProjectOptions {
        build_config: Some("Release".into()),
        build_selections: [(
            project_file.clone(),
            BuildChoice {
                config: Some("Gone".into()),
                ..BuildChoice::default()
            },
        )]
        .into_iter()
        .collect(),
        ..ProjectOptions::default()
    };

    let context = discover_webquery_at(root, &options, &OverrideSession::new(None));

    assert_eq!(context.config_selection.mode, BuildSelectionMode::Invalid);
    assert_eq!(context.config_selection.selected.as_deref(), Some("Gone"));
    assert!(context.warnings.iter().any(|warning| {
        warning
            == &format!(
                "build configuration `Gone` is not defined by {}",
                project_file.display()
            )
    }));
}

#[test]
fn invalid_session_platform_choice_has_its_own_invalid_mode() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    write_webquery_fixture(root);
    let project_file = root.join("App.dproj");
    let options = ProjectOptions {
        build_selections: [(
            project_file.clone(),
            BuildChoice {
                platform: Some("Gone".into()),
                ..BuildChoice::default()
            },
        )]
        .into_iter()
        .collect(),
        ..ProjectOptions::default()
    };

    let context = discover_webquery_at(root, &options, &OverrideSession::new(None));

    assert_eq!(context.platform_selection.mode, BuildSelectionMode::Invalid);
    assert_eq!(context.platform_selection.selected.as_deref(), Some("Gone"));
    assert!(context.warnings.iter().any(|warning| {
        warning
            == &format!(
                "platform `Gone` is not defined by {}",
                project_file.display()
            )
    }));
}

#[test]
fn invalid_config_toml_build_choice_is_reported_without_falling_back() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    write_webquery_fixture(root);
    write(
        &root.join("config.toml"),
        "[projects.\"App.dproj\"]\nconfig = 'Gone'\n",
    );
    let options = ProjectOptions {
        build_config: Some("Debug".into()),
        ..ProjectOptions::default()
    };
    let overrides = OverrideSession::new(Some(root.join("config.toml")));

    let context = discover_webquery_at(root, &options, &overrides);

    assert_eq!(context.config_selection.mode, BuildSelectionMode::Invalid);
    assert_eq!(context.config_selection.selected.as_deref(), Some("Gone"));
    assert!(context.warnings.iter().any(|warning| {
        warning
            == &format!(
                "build configuration `Gone` is not defined by {}",
                root.join("App.dproj").display()
            )
    }));
}

#[test]
fn invalid_global_build_configuration_stays_configured_and_warns() {
    let options = ProjectOptions {
        build_config: Some("Gone".into()),
        ..ProjectOptions::default()
    };
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    write_webquery_fixture(root);

    let context = discover_webquery_at(root, &options, &OverrideSession::new(None));

    assert_eq!(
        context.config_selection.mode,
        BuildSelectionMode::Configured
    );
    assert_eq!(context.config_selection.selected.as_deref(), Some("Gone"));
    assert!(context.warnings.iter().any(|warning| {
        warning
            == &format!(
                "build configuration `Gone` is not defined by {}",
                root.join("App.dproj").display()
            )
    }));
}

#[test]
fn project_selector_without_any_choice_is_a_configuration_error() {
    let temp = tempdir().expect("temporary directory");
    let config_file = temp.path().join("config.toml");
    write(&config_file, "[projects.\"App.dproj\"]\n");

    let error = OverrideSession::new(Some(config_file.clone()))
        .configuration_for(None, None)
        .expect_err("empty project selectors are rejected");

    assert!(
        error.contains("project selector `App.dproj` sets no installation, config, or platform")
    );
}

#[test]
fn project_selector_rejects_blank_build_values() {
    for (key, value) in [("config", "''"), ("platform", "''")] {
        let temp = tempdir().expect("temporary directory");
        let config_file = temp.path().join("config.toml");
        write(
            &config_file,
            &format!("[projects.\"App.dproj\"]\n{key} = {value}\n"),
        );

        let error = OverrideSession::new(Some(config_file))
            .configuration_for(None, None)
            .expect_err("blank project choices are rejected");

        assert!(error.contains(&format!("project selector `App.dproj` has an empty {key}")));
    }
}

#[test]
fn override_property_configuration_is_reported_as_configured() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    write(
        &root.join("App.dpr"),
        "program App; uses SvcMain in 'SvcMain.pas'; begin end.",
    );
    write(
        &root.join("SvcMain.pas"),
        "unit SvcMain; interface implementation end.",
    );
    write(
        &root.join("App.dproj"),
        &format!("\u{feff}{WEBQUERY_DPROJ}"),
    );
    write(
        &root.join(".delphi-tools.local.toml"),
        "[properties]\nConfig = 'Debug'\n",
    );

    let context = ProjectContext::discover_with_overrides(
        &root.join("App.dpr"),
        &[root.to_path_buf()],
        &ProjectOptions::default(),
        &OverrideSession::new(None),
    )
    .expect("WebQuery discovery with configuration override");

    assert_eq!(context.config_selection.selected.as_deref(), Some("Debug"));
    assert_eq!(
        context.config_selection.mode,
        BuildSelectionMode::Configured
    );
}

#[test]
fn standalone_contexts_have_default_build_selections() {
    let temp = tempdir().expect("temporary directory");
    let source = temp.path().join("Standalone.pas");
    write(&source, "unit Standalone; interface implementation end.");

    let context = ProjectContext::discover(
        &source,
        &[temp.path().to_path_buf()],
        &ProjectOptions::default(),
    )
    .expect("standalone discovery");

    assert_eq!(context.config_selection, BuildSelection::default());
    assert_eq!(context.platform_selection, BuildSelection::default());
}

#[test]
fn selection_modes_have_stable_wire_names() {
    assert_eq!(BuildSelectionMode::Session.as_str(), "session");
    assert_eq!(BuildSelectionMode::Configured.as_str(), "configured");
    assert_eq!(
        BuildSelectionMode::ProjectDefault.as_str(),
        "projectDefault"
    );
    assert_eq!(BuildSelectionMode::Invalid.as_str(), "invalid");
    assert_eq!(
        BuildSelectionMode::default(),
        BuildSelectionMode::ProjectDefault
    );
}

#[test]
fn project_context_recovery_visits_build_selection_strings() {
    let context = ProjectContext {
        config_selection: BuildSelection {
            selected: Some("selected-config".into()),
            candidates: vec!["candidate-config".into()],
            mode: BuildSelectionMode::Configured,
            project_default: Some("default-config".into()),
        },
        platform_selection: BuildSelection {
            selected: Some("selected-platform".into()),
            candidates: vec!["candidate-platform".into()],
            mode: BuildSelectionMode::ProjectDefault,
            project_default: Some("default-platform".into()),
        },
        ..ProjectContext::default()
    };
    let mut visited = Vec::new();

    context
        .visit_recovery_payload(&mut |bytes| {
            visited.push(bytes);
            Ok(())
        })
        .expect("recovery payload visit");

    assert_eq!(visited, [15, 16, 14, 17, 18, 16]);
}

fn discover_webquery(options: &ProjectOptions) -> ProjectContext {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    write_webquery_fixture(root);

    discover_webquery_at(root, options, &OverrideSession::new(None))
}

fn discover_webquery_at(
    root: &Path,
    options: &ProjectOptions,
    overrides: &OverrideSession,
) -> ProjectContext {
    ProjectContext::discover_with_overrides(
        &root.join("App.dpr"),
        &[root.to_path_buf()],
        options,
        overrides,
    )
    .expect("WebQuery discovery")
}

fn write_webquery_fixture(root: &Path) {
    write_webquery_fixture_with_dproj(root, WEBQUERY_DPROJ);
}

fn write_webquery_fixture_with_platforms(root: &Path) {
    let dproj = WEBQUERY_DPROJ.replace(
        "</Project>",
        "<ProjectExtensions><BorlandProject><Platforms>\
         <Platform value=\"Win32\">True</Platform><Platform value=\"Win64\">True</Platform>\
         </Platforms></BorlandProject></ProjectExtensions></Project>",
    );
    write_webquery_fixture_with_dproj(root, &dproj);
}

fn write_webquery_fixture_with_dproj(root: &Path, dproj: &str) {
    write(
        &root.join("App.dpr"),
        "program App; uses SvcMain in 'SvcMain.pas'; begin end.",
    );
    write(
        &root.join("SvcMain.pas"),
        "unit SvcMain; interface implementation end.",
    );
    write(&root.join("App.dproj"), &format!("\u{feff}{dproj}"));
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("fixture directory");
    }
    fs::write(path, contents).expect("fixture file");
}

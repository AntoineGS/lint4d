use pascal_project::build_selection::{
    BuildChoice, BuildSelection, BuildSelectionMode, parse_build_candidates,
};
use pascal_project::delphi_overrides::OverrideSession;
use pascal_project::rtl_constants::RtlConstantSource;
use pascal_project::{
    CompilerVersion, ConditionalContext, ConditionalFact, ConstantValue, OpenReason,
    ProjectContext, ProjectOptions, ProjectPathEntry, SourceOrigin,
};
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
    assert_eq!(context.platform.as_deref(), Some("Win32"));
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
fn inferred_platform_is_the_project_default_before_property_evaluation() {
    for (project_version, compiler, expected) in [
        ("23.0", Some("DCC32"), "Win32"),
        ("23.0", Some("DCC64"), "Win64"),
        ("12.0", None, "Win32"),
    ] {
        let temp = tempdir().expect("temporary directory");
        let root = temp.path();
        let compiler_property = compiler.map_or_else(String::new, |compiler| {
            format!("<DCC_DCCCompiler>{compiler}</DCC_DCCCompiler>")
        });
        let dproj = format!(
            "<Project><PropertyGroup><ProjectVersion>{project_version}</ProjectVersion>\
             <MainSource>App.dpr</MainSource><Config Condition=\"'$(Config)'==''\">Release</Config>\
             {compiler_property}</PropertyGroup><ItemGroup><BuildConfiguration Include=\"Release\"/>\
             </ItemGroup><PropertyGroup Condition=\"'$(Platform)'=='Win32'\">\
             <DCC_Define>SEEN_WIN32</DCC_Define></PropertyGroup>\
             <PropertyGroup Condition=\"'$(Platform)'=='Win64'\">\
             <DCC_Define>SEEN_WIN64</DCC_Define></PropertyGroup></Project>"
        );
        write_webquery_fixture_with_dproj(root, &dproj);
        let options = ProjectOptions {
            conditional_context: ConditionalContext::default()
                .with_compiler_version(CompilerVersion::new(35, 0)),
            ..ProjectOptions::default()
        };

        let context = discover_webquery_at(root, &options, &OverrideSession::new(None));

        assert_eq!(context.platform.as_deref(), Some(expected));
        assert_eq!(
            context.platform_selection.selected.as_deref(),
            Some(expected)
        );
        assert_eq!(
            context.platform_selection.mode,
            BuildSelectionMode::ProjectDefault
        );
        assert_eq!(
            context
                .conditional_context_for(&root.join("SvcMain.pas"))
                .define(if expected == "Win64" {
                    "SEEN_WIN64"
                } else {
                    "SEEN_WIN32"
                }),
            ConditionalFact::True,
            "platform-conditioned property group did not see inferred {expected}"
        );
    }
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

#[test]
fn project_effective_context_includes_compiler_predefined_symbols() {
    let options = ProjectOptions {
        conditional_context: ConditionalContext::default()
            .with_compiler_version(CompilerVersion::new(21, 0)),
        ..ProjectOptions::default()
    };
    let context = discover_webquery(&options);

    let conditional_context = context.effective_conditional_context();
    assert_eq!(
        conditional_context.define("MSWINDOWS"),
        ConditionalFact::True
    );
    assert_eq!(conditional_context.define("CPUX86"), ConditionalFact::False);
}

#[test]
fn project_compiled_sources_close_undefined_defines_and_keep_project_facts() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    write_webquery_fixture(root);
    let context = discover_webquery_at(root, &d2010_options(), &OverrideSession::new(None));

    assert!(context.conditional_closure.closed);
    assert!(context.conditional_closure.open_reasons.is_empty());
    assert_eq!(context.config.as_deref(), Some("Release"));
    assert_eq!(
        context.config_selection.mode,
        BuildSelectionMode::ProjectDefault
    );
    let svc = root.join("SvcMain.pas");
    assert_eq!(context.source_origin(&svc), SourceOrigin::ProjectCompiled);
    let conditionals = context.conditional_context_for(&svc);
    assert_eq!(conditionals.define("DEBUG"), ConditionalFact::False);
    assert_eq!(conditionals.define("RELEASE"), ConditionalFact::True);
    assert_eq!(conditionals.define("NOSF"), ConditionalFact::True);
    assert_eq!(conditionals.define("MSWINDOWS"), ConditionalFact::True);
    assert_eq!(conditionals.define("CPUX86"), ConditionalFact::False);
    assert_eq!(
        context.effective_conditional_context().absent_define,
        ConditionalFact::Unknown
    );
}

#[test]
fn library_sources_keep_an_open_define_world_but_receive_predefined_facts() {
    let project = tempdir().expect("project directory");
    write_webquery_fixture(project.path());
    let library_dir = tempdir().expect("library directory");
    let library_file = library_dir.path().join("LibraryUnit.pas");
    write(
        &library_file,
        "unit LibraryUnit; interface implementation end.",
    );
    let context = discover_webquery_at(
        project.path(),
        &d2010_options(),
        &OverrideSession::new(None),
    );

    assert_eq!(context.source_origin(&library_file), SourceOrigin::Library);
    let conditionals = context.conditional_context_for(&library_file);
    assert_eq!(conditionals.define("DEBUG"), ConditionalFact::Unknown);
    assert_eq!(conditionals.define("RELEASE"), ConditionalFact::Unknown);
    assert_eq!(conditionals.define("MSWINDOWS"), ConditionalFact::True);
    assert_eq!(conditionals.absent_define, ConditionalFact::Unknown);
}

#[test]
fn caller_source_paths_are_classified_as_project_compiled_roots() {
    let temp = tempdir().expect("temporary directory");
    let project = temp.path().join("project");
    let shared = temp.path().join("shared");
    write_webquery_fixture(&project);
    let shared_file = shared.join("SharedUnit.pas");
    write(
        &shared_file,
        "unit SharedUnit; interface implementation end.",
    );
    let options = ProjectOptions {
        source_paths: vec!["shared".into()],
        ..d2010_options()
    };

    let context = ProjectContext::discover_with_overrides(
        &project.join("App.dpr"),
        &[temp.path().to_path_buf()],
        &options,
        &OverrideSession::new(None),
    )
    .expect("project discovery");

    assert!(context.project_source_roots.contains(&shared));
    assert_eq!(
        context.source_origin(&shared_file),
        SourceOrigin::ProjectCompiled
    );
    assert_eq!(
        context
            .conditional_context_for(&shared_file)
            .define("DEBUG"),
        ConditionalFact::False
    );
}

#[test]
fn source_origin_uses_the_explicit_path_index_outside_project_roots() {
    let source = Path::new("/shared/SvcMain.pas");
    let context = ProjectContext {
        project_file: Some(Path::new("/project/App.dproj").to_path_buf()),
        explicit_unit_entries: [(
            "svcmain".to_owned(),
            vec![ProjectPathEntry::legacy(source.to_path_buf())],
        )]
        .into_iter()
        .collect(),
        ..ProjectContext::default()
    };

    assert_eq!(context.source_origin(source), SourceOrigin::ProjectCompiled);
}

#[test]
fn explicit_in_clause_paths_are_project_compiled_when_the_file_name_differs() {
    let temp = tempdir().expect("temporary directory");
    let project = temp.path().join("project");
    write_webquery_fixture(&project);
    write(
        &project.join("App.dpr"),
        "program App; uses SvcMain in 'SvcMain.pas', Foo in '../elsewhere/Bar.pas'; begin end.",
    );
    let renamed = temp.path().join("elsewhere/Bar.pas");
    write(&renamed, "unit Foo; interface implementation end.");

    let context = ProjectContext::discover_with_overrides(
        &project.join("App.dpr"),
        &[temp.path().to_path_buf()],
        &d2010_options(),
        &OverrideSession::new(None),
    )
    .expect("project discovery");

    assert!(context.conditional_closure.closed);
    assert_eq!(
        context.source_origin(&renamed),
        SourceOrigin::ProjectCompiled
    );
    assert_eq!(
        context.conditional_context_for(&renamed).define("DEBUG"),
        ConditionalFact::False
    );
}

#[test]
fn inferred_platform_selects_the_platform_conditioned_ide_library_path() {
    let fixture = IdeLibraryFixture::new();
    let context = fixture.discover(
        "<PropertyGroup Condition=\"'$(Platform)'=='Win32'\">\
         <DelphiLibraryPath>C:\\Shared</DelphiLibraryPath></PropertyGroup>",
        "",
        "",
    );

    assert_eq!(
        context.platform_selection.selected.as_deref(),
        Some("Win32")
    );
    assert!(
        context.search_paths.contains(&fixture.shared),
        "warnings: {:?}",
        context.warnings
    );
}

#[test]
fn ide_library_path_sources_are_project_compiled_except_inside_the_installation() {
    let fixture = IdeLibraryFixture::new();
    let shared_file = fixture.unit(&fixture.shared, "SharedUnit");
    let rtl = fixture.sdk.join("source/rtl");
    let rtl_file = fixture.unit(&rtl, "SysUtils");
    let browsing_file = fixture.unit(&fixture.sdk.join("source/vcl"), "Forms");
    let context = fixture.discover(
        "<PropertyGroup>\
         <DelphiLibraryPath>C:\\Shared;$(BDS)/source/rtl</DelphiLibraryPath>\
         <DelphiBrowsingPath>$(BDS)/source/vcl</DelphiBrowsingPath>\
         </PropertyGroup>",
        "",
        "",
    );

    assert!(
        context.conditional_closure.closed,
        "open reasons: {:?}; warnings: {:?}",
        context.conditional_closure.open_reasons, context.warnings
    );
    assert!(context.search_paths.contains(&fixture.shared));
    assert!(context.search_paths.contains(&rtl));
    assert_project_compiled(&context, &shared_file);
    assert_library(&context, &rtl_file);
    assert_library(&context, &browsing_file);
}

#[test]
fn library_path_under_an_imported_bdscommondir_stays_library_code() {
    let fixture = IdeLibraryFixture::new();
    let common = fixture.root.join("common");
    let common_file = fixture.unit(&common.join("Dcp"), "CommonUnit");
    let context = fixture.discover(
        "<PropertyGroup>\
         <DelphiLibraryPath>C:\\Shared;$(BDSCOMMONDIR)\\Dcp</DelphiLibraryPath>\
         </PropertyGroup>",
        "SET BDSCOMMONDIR=C:\\Common\n",
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\\\Common'\nto = '{}'\n",
            common.display()
        ),
    );

    assert!(
        context.search_paths.contains(&common.join("Dcp")),
        "warnings: {:?}",
        context.warnings
    );
    assert_library(&context, &common_file);
}

#[test]
fn library_path_under_an_unmapped_installation_root_stays_library_code() {
    let fixture = IdeLibraryFixture::new();
    let mapped_source = fixture.root.join("mapped-sdk-source");
    let source_file = fixture.unit(&mapped_source, "SysUtils");
    let context = fixture.discover_with_bds(
        "C:\\SDK",
        "<PropertyGroup>\
         <DelphiLibraryPath>C:\\Shared;C:\\SDK\\source</DelphiLibraryPath>\
         </PropertyGroup>",
        &format!(
            "[[path_mappings]]\nfrom = 'C:\\\\SDK\\\\source'\nto = '{}'\n",
            mapped_source.display()
        ),
    );

    assert!(
        context.search_paths.contains(&mapped_source),
        "warnings: {:?}",
        context.warnings
    );
    assert_library(&context, &source_file);
}

struct IdeLibraryFixture {
    _temp: tempfile::TempDir,
    root: std::path::PathBuf,
    project: std::path::PathBuf,
    shared: std::path::PathBuf,
    sdk: std::path::PathBuf,
    appdata: std::path::PathBuf,
}

impl IdeLibraryFixture {
    fn new() -> Self {
        let temp = tempdir().expect("temporary directory");
        let root = temp.path().to_path_buf();
        let fixture = Self {
            project: root.join("project"),
            shared: root.join("shared"),
            sdk: root.join("sdk"),
            appdata: root.join("appdata"),
            root,
            _temp: temp,
        };
        write_webquery_fixture(&fixture.project);
        fs::create_dir_all(&fixture.shared).expect("library directory");
        fixture
    }

    fn unit(&self, directory: &Path, name: &str) -> std::path::PathBuf {
        let file = directory.join(format!("{name}.pas"));
        write(
            &file,
            &format!("unit {name}; interface implementation end."),
        );
        file
    }

    fn discover(&self, env_options: &str, rsvars_extra: &str, toml_extra: &str) -> ProjectContext {
        write(
            &self.sdk.join("bin/rsvars.bat"),
            &format!("SET BDS={}\n{rsvars_extra}", self.sdk.display()),
        );
        let bds = self.sdk.display().to_string();
        self.discover_with_bds(&bds, env_options, toml_extra)
    }

    fn discover_with_bds(&self, bds: &str, env_options: &str, toml_extra: &str) -> ProjectContext {
        write(
            &self.appdata.join("EnvOptions.proj"),
            &format!("<Project>{env_options}</Project>"),
        );
        write(
            &self.root.join(".delphi-tools.local.toml"),
            &format!(
                "[installations.\"37.0\".properties]\nBDS='{bds}'\nAPPDATA='{}'\n\
                 [[path_mappings]]\nfrom = 'C:\\\\Shared'\nto = '{}'\n{toml_extra}\
                 [projects.\"project/App.dproj\"]\ninstallation='37.0'\n",
                self.appdata.display(),
                self.shared.display()
            ),
        );
        ProjectContext::discover_with_overrides(
            &self.project.join("App.dpr"),
            std::slice::from_ref(&self.root),
            &ProjectOptions::default(),
            &OverrideSession::new(None),
        )
        .expect("project discovery")
    }
}

fn assert_project_compiled(context: &ProjectContext, file: &Path) {
    assert_eq!(context.source_origin(file), SourceOrigin::ProjectCompiled);
    assert_eq!(
        context.conditional_context_for(file).define("DEBUG"),
        ConditionalFact::False
    );
}

fn assert_library(context: &ProjectContext, file: &Path) {
    assert_eq!(context.source_origin(file), SourceOrigin::Library);
    assert_eq!(
        context.conditional_context_for(file).define("DEBUG"),
        ConditionalFact::Unknown
    );
}

#[test]
fn project_define_wins_over_a_conflicting_predefined_symbol() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    let dproj = WEBQUERY_DPROJ.replace(
        "<DCC_Define>RELEASE;$(DCC_Define)</DCC_Define>",
        "<DCC_Define>WIN64;RELEASE;$(DCC_Define)</DCC_Define>",
    );
    write_webquery_fixture_with_dproj(root, &dproj);
    let context = discover_webquery_at(root, &d2010_options(), &OverrideSession::new(None));

    assert_eq!(
        context
            .conditional_context_for(&root.join("SvcMain.pas"))
            .define("WIN64"),
        ConditionalFact::True
    );
}

#[test]
fn client_conditional_fact_wins_over_project_and_predefined_facts() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    let dproj = WEBQUERY_DPROJ.replace(
        "<DCC_Define>RELEASE;$(DCC_Define)</DCC_Define>",
        "<DCC_Define>MSWINDOWS;RELEASE;$(DCC_Define)</DCC_Define>",
    );
    write_webquery_fixture_with_dproj(root, &dproj);
    let mut conditional_context =
        ConditionalContext::default().with_compiler_version(CompilerVersion::new(21, 0));
    conditional_context.set_define("MSWINDOWS", ConditionalFact::False);
    let options = ProjectOptions {
        conditional_context,
        ..ProjectOptions::default()
    };

    let context = discover_webquery_at(root, &options, &OverrideSession::new(None));

    assert_eq!(
        context
            .conditional_context_for(&root.join("SvcMain.pas"))
            .define("MSWINDOWS"),
        ConditionalFact::False
    );
}

#[test]
fn missing_compiler_version_keeps_project_defines_open() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    write_webquery_fixture(root);
    let context = discover_webquery_at(
        root,
        &ProjectOptions::default(),
        &OverrideSession::new(None),
    );

    assert!(!context.conditional_closure.closed);
    assert!(
        context
            .conditional_closure
            .open_reasons
            .contains(&OpenReason::CompilerVersionUnknown)
    );
    assert_eq!(
        context
            .conditional_context_for(&root.join("SvcMain.pas"))
            .define("DEBUG"),
        ConditionalFact::Unknown
    );
}

#[test]
fn dpr_and_dpk_projects_keep_absent_defines_unknown_without_a_dproj() {
    for (extension, source) in [
        ("dpr", "program App; begin end."),
        ("dpk", "package App; end."),
    ] {
        let temp = tempdir().expect("temporary directory");
        let root = temp.path();
        let project = root.join(format!("App.{extension}"));
        write(&project, source);
        let options = ProjectOptions {
            build_config: Some("Release".into()),
            platform: Some("Win32".into()),
            conditional_context: ConditionalContext::default()
                .with_compiler_version(CompilerVersion::new(35, 0)),
            ..ProjectOptions::default()
        };

        let context = ProjectContext::discover_with_overrides(
            &project,
            &[root.to_path_buf()],
            &options,
            &OverrideSession::new(None),
        )
        .expect("Pascal project discovery");
        let conditionals = context.conditional_context_for(&project);

        assert!(
            !context.conditional_closure.closed,
            "{extension} closed the define world"
        );
        assert!(
            context
                .conditional_closure
                .open_reasons
                .contains(&OpenReason::NoProject)
        );
        assert_eq!(conditionals.absent_define, ConditionalFact::Unknown);
        assert_eq!(
            conditionals.define("UNLISTED_BUILD_DEFINE"),
            ConditionalFact::Unknown
        );
        assert_eq!(conditionals.define("MSWINDOWS"), ConditionalFact::True);
    }
}

#[test]
fn system_pas_rtl_constants_are_tracked_and_added_to_contexts() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    let dproj = WEBQUERY_DPROJ.replace(
        "<DCC_DCCCompiler>DCC32</DCC_DCCCompiler>",
        "<DCC_DCCCompiler>DCC32</DCC_DCCCompiler><DCC_UnitSearchPath>rtl</DCC_UnitSearchPath>",
    );
    write_webquery_fixture_with_dproj(root, &dproj);
    let system_pas = root.join("rtl/System.pas");
    write(
        &system_pas,
        "unit System; interface const RTLVersion111 = True; implementation end.",
    );
    let library_dir = tempdir().expect("library directory");
    let library_file = library_dir.path().join("LibraryUnit.pas");
    write(
        &library_file,
        "unit LibraryUnit; interface implementation end.",
    );
    let options = ProjectOptions {
        conditional_context: ConditionalContext::default()
            .with_compiler_version(CompilerVersion::new(35, 0)),
        ..ProjectOptions::default()
    };

    let context = discover_webquery_at(root, &options, &OverrideSession::new(None));

    assert_eq!(
        context.conditional_closure.rtl_source,
        RtlConstantSource::SystemPas
    );
    assert!(
        context
            .conditional_closure
            .rtl_constants
            .as_ref()
            .is_some_and(|names| names.iter().any(|name| name == "RTLVersion111"))
    );
    assert!(context.conditional_context.rtl_constants_known);
    assert_eq!(
        context
            .conditional_context_for(&root.join("SvcMain.pas"))
            .constants
            .get("RTLVERSION111"),
        Some(&ConstantValue::Boolean(true))
    );
    assert_eq!(
        context
            .conditional_context_for(&library_file)
            .constants
            .get("RTLVERSION111"),
        Some(&ConstantValue::Boolean(true))
    );
    assert!(
        context
            .metadata_observations
            .iter()
            .any(|observation| observation.path() == system_pas)
    );
    assert!(context.metadata_files.contains(&system_pas));
    assert!(context.project_source_roots.contains(&root.join("rtl")));
    assert_eq!(
        context.source_origin(&system_pas),
        SourceOrigin::ProjectCompiled
    );
}

#[test]
fn inconclusive_system_pas_declarations_warn_and_use_the_version_table() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    let dproj = WEBQUERY_DPROJ.replace(
        "<DCC_DCCCompiler>DCC32</DCC_DCCCompiler>",
        "<DCC_DCCCompiler>DCC32</DCC_DCCCompiler><DCC_UnitSearchPath>rtl</DCC_UnitSearchPath>",
    );
    write_webquery_fixture_with_dproj(root, &dproj);
    write(
        &root.join("rtl/System.pas"),
        "unit System; interface const RTLVersion999: Boolean = True; implementation end.",
    );
    let options = ProjectOptions {
        conditional_context: ConditionalContext::default()
            .with_compiler_version(CompilerVersion::new(35, 0)),
        ..ProjectOptions::default()
    };

    let context = discover_webquery_at(root, &options, &OverrideSession::new(None));

    assert_eq!(
        context.conditional_closure.rtl_source,
        RtlConstantSource::Table
    );
    assert!(
        context
            .warnings
            .iter()
            .any(|warning| { warning.contains("System.pas RTL constant scan was inconclusive") })
    );
    let conditionals = context.conditional_context_for(&root.join("SvcMain.pas"));
    assert_eq!(
        conditionals.constants.get("RTLVERSION113"),
        Some(&ConstantValue::Boolean(true))
    );
    assert_eq!(
        conditionals.constants.get("RTLVERSION999"),
        None,
        "unsupported declarations must not enter the complete table fallback"
    );
    assert!(conditionals.rtl_constants_known);
}

#[test]
fn valid_rtl_override_skips_system_pas_search_entirely() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    let dproj = WEBQUERY_DPROJ.replace(
        "<DCC_DCCCompiler>DCC32</DCC_DCCCompiler>",
        "<DCC_DCCCompiler>DCC32</DCC_DCCCompiler><DCC_UnitSearchPath>missing-rtl</DCC_UnitSearchPath>",
    );
    write_webquery_fixture_with_dproj(root, &dproj);
    let config = root.join("config.toml");
    write(
        &config,
        "[installations.\"35.0\"]\nrtlVersionConstants = [\"RTLVersion111\"]\n",
    );
    let options = ProjectOptions {
        installation_selections: [(root.join("App.dproj"), "35.0".into())]
            .into_iter()
            .collect(),
        conditional_context: ConditionalContext::default()
            .with_compiler_version(CompilerVersion::new(35, 0)),
        ..ProjectOptions::default()
    };

    let context = discover_webquery_at(root, &options, &OverrideSession::new(Some(config)));

    assert_eq!(
        context.conditional_closure.rtl_source,
        RtlConstantSource::Override
    );
    assert!(
        !context
            .warnings
            .iter()
            .any(|warning| warning.contains("for System.pas"))
    );
}

#[test]
fn compiler_before_104_skips_system_pas_search() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    let dproj = WEBQUERY_DPROJ.replace(
        "<DCC_DCCCompiler>DCC32</DCC_DCCCompiler>",
        "<DCC_DCCCompiler>DCC32</DCC_DCCCompiler><DCC_UnitSearchPath>missing-rtl</DCC_UnitSearchPath>",
    );
    write_webquery_fixture_with_dproj(root, &dproj);
    let options = ProjectOptions {
        conditional_context: ConditionalContext::default()
            .with_compiler_version(CompilerVersion::new(33, 0)),
        ..ProjectOptions::default()
    };

    let context = discover_webquery_at(root, &options, &OverrideSession::new(None));

    assert_eq!(
        context.conditional_closure.rtl_source,
        RtlConstantSource::Table
    );
    assert!(
        context
            .conditional_closure
            .rtl_constants
            .unwrap()
            .is_empty()
    );
    assert!(
        !context
            .warnings
            .iter()
            .any(|warning| warning.contains("for System.pas"))
    );
}

#[test]
fn console_predefined_symbol_comes_from_main_source_directive() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    write_webquery_fixture(root);
    write(
        &root.join("App.dpr"),
        "program App; {$APPTYPE\n CONSOLE} uses SvcMain; begin end.",
    );
    let context = discover_webquery_at(root, &d2010_options(), &OverrideSession::new(None));
    assert_eq!(
        context
            .conditional_context_for(&root.join("SvcMain.pas"))
            .define("CONSOLE"),
        ConditionalFact::True
    );

    write(
        &root.join("App.dpr"),
        "program App; uses SvcMain; begin end.",
    );
    let plain_context = discover_webquery_at(root, &d2010_options(), &OverrideSession::new(None));
    assert_eq!(
        plain_context
            .conditional_context_for(&root.join("SvcMain.pas"))
            .define("CONSOLE"),
        ConditionalFact::False
    );
}

#[test]
fn console_predefined_symbol_ignores_directive_text_in_comments_and_strings() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    write_webquery_fixture(root);
    write(
        &root.join("App.dpr"),
        "program App; // {$APPTYPE CONSOLE}\n uses SvcMain; begin WriteLn('{$APPTYPE CONSOLE}'); end.",
    );
    let context = discover_webquery_at(root, &d2010_options(), &OverrideSession::new(None));

    assert_eq!(
        context
            .conditional_context_for(&root.join("SvcMain.pas"))
            .define("CONSOLE"),
        ConditionalFact::False
    );
}

#[test]
fn console_predefined_symbol_recognizes_parenthesized_apptype_directives() {
    for directive in ["(*$APPTYPE CONSOLE*)", "(*$ apptype   console *)"] {
        let temp = tempdir().expect("temporary directory");
        let root = temp.path();
        write_webquery_fixture(root);
        write(
            &root.join("App.dpr"),
            &format!("program App; {directive} uses SvcMain; begin end."),
        );
        let context = discover_webquery_at(root, &d2010_options(), &OverrideSession::new(None));

        assert_eq!(
            context
                .conditional_context_for(&root.join("SvcMain.pas"))
                .define("CONSOLE"),
            ConditionalFact::True,
            "failed to recognize {directive}"
        );
    }
}

#[test]
fn standalone_contexts_remain_open_and_use_client_predefined_symbols() {
    let temp = tempdir().expect("temporary directory");
    let source = temp.path().join("Standalone.pas");
    write(&source, "unit Standalone; interface implementation end.");
    let options = ProjectOptions {
        platform: Some("Win32".into()),
        conditional_context: ConditionalContext::default()
            .with_compiler_version(CompilerVersion::new(21, 0)),
        ..ProjectOptions::default()
    };
    let context = ProjectContext::discover_with_overrides(
        &source,
        &[temp.path().to_path_buf()],
        &options,
        &OverrideSession::new(None),
    )
    .expect("standalone discovery");

    assert!(!context.conditional_closure.closed);
    assert!(
        context
            .conditional_closure
            .open_reasons
            .contains(&OpenReason::NoProject)
    );
    assert_eq!(
        context.conditional_context_for(&source).define("MSWINDOWS"),
        ConditionalFact::True
    );
}

#[test]
fn standalone_pas_sources_do_not_infer_console_target() {
    let temp = tempdir().expect("temporary directory");
    let source = temp.path().join("Standalone.pas");
    write(&source, "unit Standalone; interface implementation end.");

    let context = ProjectContext::discover_with_overrides(
        &source,
        &[temp.path().to_path_buf()],
        &ProjectOptions::default(),
        &OverrideSession::new(None),
    )
    .expect("standalone discovery");

    assert_eq!(
        context.conditional_context_for(&source).define("CONSOLE"),
        ConditionalFact::Unknown
    );
    assert!(
        context
            .metadata_observations
            .iter()
            .all(|observation| observation.path() != source),
        "standalone source must not be recorded as a metadata observation"
    );
}

#[test]
fn standalone_context_keeps_version_symbols_when_platform_is_unknown() {
    let temp = tempdir().expect("temporary directory");
    let source = temp.path().join("Standalone.pas");
    write(&source, "unit Standalone; interface implementation end.");
    let options = ProjectOptions {
        conditional_context: ConditionalContext::default()
            .with_compiler_version(CompilerVersion::new(35, 0)),
        ..ProjectOptions::default()
    };
    let context = ProjectContext::discover_with_overrides(
        &source,
        &[temp.path().to_path_buf()],
        &options,
        &OverrideSession::new(None),
    )
    .expect("standalone discovery");
    let conditionals = context.conditional_context_for(&source);

    assert_eq!(conditionals.define("VER350"), ConditionalFact::True);
    assert_eq!(conditionals.define("UNICODE"), ConditionalFact::True);
    assert_eq!(conditionals.define("MSWINDOWS"), ConditionalFact::Unknown);
}

#[test]
fn dcc_and_nativecode_are_version_facts_on_other_platforms() {
    let temp = tempdir().expect("temporary directory");
    let source = temp.path().join("Standalone.pas");
    write(&source, "unit Standalone; interface implementation end.");
    let options = ProjectOptions {
        platform: Some("Linux".into()),
        conditional_context: ConditionalContext::default()
            .with_compiler_version(CompilerVersion::new(35, 0)),
        ..ProjectOptions::default()
    };
    let context = ProjectContext::discover_with_overrides(
        &source,
        &[temp.path().to_path_buf()],
        &options,
        &OverrideSession::new(None),
    )
    .expect("standalone discovery");
    let conditionals = context.conditional_context_for(&source);

    assert_eq!(conditionals.define("DCC"), ConditionalFact::True);
    assert_eq!(conditionals.define("NATIVECODE"), ConditionalFact::True);
    assert_eq!(conditionals.define("MSWINDOWS"), ConditionalFact::Unknown);
}

#[test]
fn invalid_build_selection_is_an_explicit_open_reason() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    write_webquery_fixture(root);
    let options = ProjectOptions {
        build_selections: [(
            root.join("App.dproj"),
            BuildChoice {
                config: Some("Gone".into()),
                ..BuildChoice::default()
            },
        )]
        .into_iter()
        .collect(),
        conditional_context: ConditionalContext::default()
            .with_compiler_version(CompilerVersion::new(21, 0)),
        ..ProjectOptions::default()
    };

    let context = discover_webquery_at(root, &options, &OverrideSession::new(None));

    assert!(!context.conditional_closure.closed);
    assert!(
        context
            .conditional_closure
            .open_reasons
            .contains(&OpenReason::ConfigInvalid)
    );
    assert_eq!(OpenReason::ConfigInvalid.as_str(), "configInvalid");
}

#[test]
fn configured_value_outside_project_candidates_keeps_conditionals_open() {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    write_webquery_fixture(root);
    let options = ProjectOptions {
        build_config: Some("Gone".into()),
        platform: Some("Win32".into()),
        ..d2010_options()
    };

    let context = discover_webquery_at(root, &options, &OverrideSession::new(None));

    assert_eq!(
        context.config_selection.mode,
        BuildSelectionMode::Configured
    );
    assert!(!context.conditional_closure.closed);
    assert!(
        context
            .conditional_closure
            .open_reasons
            .contains(&OpenReason::DiscoveryIncomplete),
        "unexpected open reasons: {:?}",
        context.conditional_closure.open_reasons
    );
    assert_eq!(
        context
            .conditional_context_for(&root.join("SvcMain.pas"))
            .define("DEBUG"),
        ConditionalFact::Unknown
    );
}

#[test]
fn empty_configuration_candidates_leave_each_selection_source_unverified() {
    const NO_CONFIG_CANDIDATES: &str = r#"<Project>
      <PropertyGroup>
        <ProjectVersion>21.0</ProjectVersion>
        <MainSource>App.dpr</MainSource>
        <Config Condition="'$(Config)'==''">Debug</Config>
        <DCC_DCCCompiler>DCC32</DCC_DCCCompiler>
      </PropertyGroup>
      <ItemGroup>
        <DelphiCompile Include="App.dpr"><MainSource>MainSource</MainSource></DelphiCompile>
        <DCCReference Include="SvcMain.pas" />
      </ItemGroup>
    </Project>"#;

    for (origin, expected_mode) in [
        (
            EmptyCandidatesSelection::Session,
            BuildSelectionMode::Session,
        ),
        (
            EmptyCandidatesSelection::ConfigToml,
            BuildSelectionMode::Configured,
        ),
        (
            EmptyCandidatesSelection::Client,
            BuildSelectionMode::Configured,
        ),
    ] {
        let temp = tempdir().expect("temporary workspace");
        let root = temp.path();
        write_webquery_fixture_with_dproj(root, NO_CONFIG_CANDIDATES);
        let (options, overrides) = empty_candidate_selection_input(root, origin, false);

        let context = discover_webquery_at(root, &options, &overrides);

        assert!(context.config_selection.candidates.is_empty());
        assert_eq!(context.config_selection.selected.as_deref(), Some("Debug"));
        assert_eq!(context.config_selection.mode, expected_mode);
        assert!(
            !context
                .warnings
                .iter()
                .any(|warning| { warning.contains("build configuration `Debug` is not defined") })
        );
        assert!(
            !context
                .conditional_closure
                .open_reasons
                .contains(&OpenReason::ConfigInvalid)
        );
        assert!(
            context
                .conditional_closure
                .open_reasons
                .contains(&OpenReason::DiscoveryIncomplete)
        );
        assert!(!context.conditional_closure.closed);
        assert_eq!(
            context
                .conditional_context_for(&root.join("SvcMain.pas"))
                .define("UNLISTED_BUILD_DEFINE"),
            ConditionalFact::Unknown
        );
    }
}

#[test]
fn empty_platform_candidates_leave_each_selection_source_unverified() {
    const NO_PLATFORM_CANDIDATES: &str = r#"<Project>
      <PropertyGroup>
        <MainSource>App.dpr</MainSource>
      </PropertyGroup>
      <ItemGroup>
        <DelphiCompile Include="App.dpr"><MainSource>MainSource</MainSource></DelphiCompile>
        <DCCReference Include="SvcMain.pas" />
        <BuildConfiguration Include="Debug" />
      </ItemGroup>
    </Project>"#;

    for (origin, expected_mode) in [
        (
            EmptyCandidatesSelection::Session,
            BuildSelectionMode::Session,
        ),
        (
            EmptyCandidatesSelection::ConfigToml,
            BuildSelectionMode::Configured,
        ),
        (
            EmptyCandidatesSelection::Client,
            BuildSelectionMode::Configured,
        ),
    ] {
        let temp = tempdir().expect("temporary workspace");
        let root = temp.path();
        write_webquery_fixture_with_dproj(root, NO_PLATFORM_CANDIDATES);
        let (options, overrides) = empty_candidate_selection_input(root, origin, true);

        let context = discover_webquery_at(root, &options, &overrides);

        assert_eq!(context.config_selection.candidates, ["Debug"]);
        assert!(context.platform_selection.candidates.is_empty());
        assert_eq!(
            context.platform_selection.selected.as_deref(),
            Some("Win32")
        );
        assert_eq!(context.platform_selection.mode, expected_mode);
        assert!(
            !context
                .warnings
                .iter()
                .any(|warning| warning.contains("platform `Win32` is not defined"))
        );
        assert!(
            !context
                .conditional_closure
                .open_reasons
                .contains(&OpenReason::PlatformInvalid)
        );
        assert!(
            context
                .conditional_closure
                .open_reasons
                .contains(&OpenReason::DiscoveryIncomplete)
        );
        assert!(!context.conditional_closure.closed);
        assert_eq!(
            context
                .conditional_context_for(&root.join("SvcMain.pas"))
                .define("UNLISTED_BUILD_DEFINE"),
            ConditionalFact::Unknown
        );
    }
}

#[test]
fn removing_last_build_candidate_keeps_session_and_config_choices_open() {
    const DPROJ: &str = r#"<Project>
      <PropertyGroup>
        <ProjectVersion>12.0</ProjectVersion>
        <MainSource>App.dpr</MainSource>
        <Config Condition="'$(Config)'==''">Debug</Config>
        <Platform Condition="'$(Platform)'==''">Win32</Platform>
        <DCC_DCCCompiler>DCC32</DCC_DCCCompiler>
      </PropertyGroup>
      <ItemGroup>
        <DelphiCompile Include="App.dpr"><MainSource>MainSource</MainSource></DelphiCompile>
        <DCCReference Include="SvcMain.pas" />
        <BuildConfiguration Include="Debug" />
      </ItemGroup>
      <ProjectExtensions><BorlandProject><Platforms>
        <Platform value="Win32">True</Platform>
      </Platforms></BorlandProject></ProjectExtensions>
    </Project>"#;

    for is_platform in [false, true] {
        for origin in [
            EmptyCandidatesSelection::Session,
            EmptyCandidatesSelection::ConfigToml,
        ] {
            let temp = tempdir().expect("temporary workspace");
            let root = temp.path();
            write_webquery_fixture_with_dproj(root, DPROJ);
            let (options, overrides) = empty_candidate_selection_input(root, origin, is_platform);
            let before = discover_webquery_at(root, &options, &overrides);
            assert!(before.conditional_closure.closed);

            let removed_candidate = if is_platform {
                "<Platform value=\"Win32\">True</Platform>"
            } else {
                "<BuildConfiguration Include=\"Debug\" />"
            };
            let without_candidate = DPROJ.replace(removed_candidate, "");
            assert_ne!(without_candidate, DPROJ);
            fs::write(root.join("App.dproj"), without_candidate)
                .expect("remove the final declared candidate");

            let after = discover_webquery_at(root, &options, &overrides);
            let selection = if is_platform {
                &after.platform_selection
            } else {
                &after.config_selection
            };
            assert!(selection.candidates.is_empty());
            assert_eq!(
                selection.mode,
                match origin {
                    EmptyCandidatesSelection::Session => BuildSelectionMode::Session,
                    EmptyCandidatesSelection::ConfigToml => BuildSelectionMode::Configured,
                    EmptyCandidatesSelection::Client => unreachable!(),
                }
            );
            assert!(!after.conditional_closure.closed);
            assert!(
                after
                    .conditional_closure
                    .open_reasons
                    .contains(&OpenReason::DiscoveryIncomplete)
            );
            assert_eq!(
                after
                    .conditional_context_for(&root.join("SvcMain.pas"))
                    .define("UNLISTED_BUILD_DEFINE"),
                ConditionalFact::Unknown
            );
        }
    }
}

fn discover_webquery(options: &ProjectOptions) -> ProjectContext {
    let temp = tempdir().expect("temporary directory");
    let root = temp.path();
    write_webquery_fixture(root);

    discover_webquery_at(root, options, &OverrideSession::new(None))
}

fn d2010_options() -> ProjectOptions {
    ProjectOptions {
        conditional_context: ConditionalContext::default()
            .with_compiler_version(CompilerVersion::new(21, 0)),
        ..ProjectOptions::default()
    }
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

#[derive(Clone, Copy)]
enum EmptyCandidatesSelection {
    Session,
    ConfigToml,
    Client,
}

fn empty_candidate_selection_input(
    root: &Path,
    origin: EmptyCandidatesSelection,
    is_platform: bool,
) -> (ProjectOptions, OverrideSession) {
    let value = if is_platform { "Win32" } else { "Debug" };
    let field = if is_platform { "platform" } else { "config" };
    let mut options = d2010_options();
    let mut overrides = OverrideSession::new(None);
    match origin {
        EmptyCandidatesSelection::Session => {
            let mut choice = BuildChoice::default();
            if is_platform {
                choice.platform = Some(value.to_owned());
            } else {
                choice.config = Some(value.to_owned());
            }
            options
                .build_selections
                .insert(root.join("App.dproj"), choice);
        }
        EmptyCandidatesSelection::ConfigToml => {
            let config_file = root.join("config.toml");
            write(
                &config_file,
                &format!("[projects.\"App.dproj\"]\n{field} = '{value}'\n"),
            );
            overrides = OverrideSession::new(Some(config_file));
        }
        EmptyCandidatesSelection::Client => {
            if is_platform {
                options.platform = Some(value.to_owned());
            } else {
                options.build_config = Some(value.to_owned());
            }
        }
    }
    (options, overrides)
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

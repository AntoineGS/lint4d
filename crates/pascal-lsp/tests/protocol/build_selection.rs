use super::*;

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
		<DCCReference Include="SvcMain.pas" />
		<DCCReference Include="Helper.pas" />
		<BuildConfiguration Include="Base"><Key>Base</Key></BuildConfiguration>
		<BuildConfiguration Include="Debug"><Key>Cfg_2</Key><CfgParent>Base</CfgParent></BuildConfiguration>
		<BuildConfiguration Include="Release"><Key>Cfg_1</Key><CfgParent>Base</CfgParent></BuildConfiguration>
	</ItemGroup>
</Project>"#;

const APP_SOURCE: &str =
    "program App; uses SvcMain in 'SvcMain.pas', Helper in 'Helper.pas'; begin end.\n";
const HELPER_SOURCE: &str =
    "unit Helper; interface procedure Help; implementation procedure Help; begin end; end.\n";
const SERVICE_SOURCE: &str = "unit SvcMain;\ninterface\nuses Helper;\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\n  {$IFDEF DEBUG}\n  Help;\n  {$ENDIF}\n  Help;\nend;\nend.\n";

fn webquery_fixture(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let project = root.join("App.dproj");
    let app = root.join("App.dpr");
    let service = root.join("SvcMain.pas");
    write_file(&project, WEBQUERY_DPROJ);
    write_file(&app, APP_SOURCE);
    write_file(&service, SERVICE_SOURCE);
    write_file(&root.join("Helper.pas"), HELPER_SOURCE);
    (project, app, service)
}

fn request_build_context(server: &mut TestServer, project: &Path, suffix: &str) -> Value {
    let id = RequestId::from(format!("build-context-{suffix}"));
    server.send_request(
        id.clone(),
        "pascal/buildContext",
        json!({"projectUri": uri(project)}),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    response.result.expect("build context result")
}

#[test]
fn build_context_and_configuration_selection_use_project_defaults_and_refresh_navigation() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let (project, _app, service) = webquery_fixture(root);
    let mut server = TestServer::launch();
    let initialize = server.initialize(root, json!({"compilerVersion": "21.0"}));
    assert_eq!(
        initialize["capabilities"]["experimental"]["buildSelection"],
        true
    );

    let context = request_build_context(&mut server, &project, "initial");
    assert_eq!(
        context["projectUri"],
        uri(&project).as_str(),
        "response uses canonical project URI"
    );
    assert_eq!(
        context["config"],
        json!({
            "selected": "Release",
            "candidates": ["Debug", "Release"],
            "mode": "projectDefault",
            "projectDefault": "Release"
        })
    );
    assert_eq!(
        context["platform"],
        json!({
            "selected": "Win32",
            "candidates": ["Win32"],
            "mode": "projectDefault",
            "projectDefault": "Win32"
        })
    );
    assert_eq!(
        context["conditionals"],
        json!({
            "closed": true,
            "openReasons": [],
            "rtlVersionConstants": {"source": "table", "names": []}
        })
    );

    let select_debug_id = RequestId::from("build-select-debug".to_owned());
    server.send_request(
        select_debug_id.clone(),
        "pascal/selectBuildConfig",
        json!({"projectUri": uri(&project), "config": "Debug"}),
    );
    let select_debug = server.response(&select_debug_id);
    assert!(select_debug.error.is_none(), "{select_debug:?}");
    let selected_debug = select_debug.result.expect("selected debug context");
    assert_eq!(selected_debug["config"]["selected"], "Debug");
    assert_eq!(selected_debug["config"]["mode"], "session");

    let definition_id = RequestId::from("build-debug-definition".to_owned());
    server.send_request(
        definition_id.clone(),
        "textDocument/definition",
        navigation_params(&service, SERVICE_SOURCE, "Help;", 0),
    );
    let definition = result_locations(server.response(&definition_id));
    assert_eq!(definition.len(), 1);
    assert_eq!(
        definition[0]["uri"],
        uri(&root.join("Helper.pas")).to_string()
    );

    let select_release_id = RequestId::from("build-select-release".to_owned());
    server.send_request(
        select_release_id.clone(),
        "pascal/selectBuildConfig",
        json!({"projectUri": uri(&project), "config": "Release"}),
    );
    let select_release = server.response(&select_release_id);
    assert!(select_release.error.is_none(), "{select_release:?}");
    assert_eq!(select_release.result.unwrap()["config"]["mode"], "session");

    let select_invalid_id = RequestId::from("build-select-invalid".to_owned());
    server.send_request(
        select_invalid_id.clone(),
        "pascal/selectBuildConfig",
        json!({"projectUri": uri(&project), "config": "Nope"}),
    );
    let invalid = server.response(&select_invalid_id);
    assert_eq!(
        invalid.error.as_ref().map(|error| error.message.as_str()),
        Some("unknown build configuration `Nope`")
    );
    let unchanged = request_build_context(&mut server, &project, "after-invalid");
    assert_eq!(unchanged["config"]["selected"], "Release");
    assert_eq!(unchanged["config"]["mode"], "session");

    let reset_id = RequestId::from("build-reset-config".to_owned());
    server.send_request(
        reset_id.clone(),
        "pascal/selectBuildConfig",
        json!({"projectUri": uri(&project), "config": null}),
    );
    let reset = server.response(&reset_id);
    assert!(reset.error.is_none(), "{reset:?}");
    let reset = reset.result.expect("reset build context");
    assert_eq!(reset["config"]["selected"], "Release");
    assert_eq!(reset["config"]["mode"], "projectDefault");

    let missing_id = RequestId::from("build-missing-config".to_owned());
    server.send_request(
        missing_id.clone(),
        "pascal/selectBuildConfig",
        json!({"projectUri": uri(&project)}),
    );
    let missing = server.response(&missing_id);
    assert_eq!(missing.error.as_ref().map(|error| error.code), Some(-32602));
    assert_eq!(
        missing.error.as_ref().map(|error| error.message.as_str()),
        Some("config is required; use null to reset the session selection")
    );
    server.shutdown();
}

#[test]
fn build_platform_selection_rejects_platforms_not_in_the_project() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let (project, _, _) = webquery_fixture(root);
    let mut server = TestServer::launch();
    server.initialize(root, json!({"compilerVersion": "21.0"}));

    let id = RequestId::from("build-select-win64".to_owned());
    server.send_request(
        id.clone(),
        "pascal/selectPlatform",
        json!({"projectUri": uri(&project), "platform": "Win64"}),
    );
    let response = server.response(&id);
    assert_eq!(
        response.error.as_ref().map(|error| error.message.as_str()),
        Some("unknown platform `Win64`")
    );
    server.shutdown();
}

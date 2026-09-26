use super::*;

struct SelectionFixture {
    directory: tempfile::TempDir,
    project_a: std::path::PathBuf,
    project_b: std::path::PathBuf,
    main_a: std::path::PathBuf,
}

fn selection_fixture() -> SelectionFixture {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let mut config = String::new();
    for version in ["7.0", "37.0"] {
        let sdk = root.join("sdk").join(version);
        let ide = root.join("ide").join(version);
        write_file(
            &sdk.join("bin/rsvars.bat"),
            &format!("@SET BDS=C:\\SDK\\{version}\n"),
        );
        let property = if version == "7.0" {
            "Win32LibraryPath"
        } else {
            "DelphiLibraryPath"
        };
        write_file(
            &ide.join("EnvOptions.proj"),
            &format!(
                "<Project><PropertyGroup><{property}>$(BDS)\\source</{property}></PropertyGroup></Project>"
            ),
        );
        write_file(
            &sdk.join("source/SdkUnit.pas"),
            &format!(
                "unit SdkUnit; interface const SdkValue = {}; implementation end.",
                if version == "7.0" { 7 } else { 37 },
            ),
        );
        config.push_str(&format!(
            "[installations.\"{version}\".properties]\nBDS='{}'\nAPPDATA='{}'\n",
            sdk.display(),
            ide.display(),
        ));
    }
    for (name, version) in [("a", "7.0"), ("b", "37.0")] {
        write_file(
            &root.join(name).join("App.dproj"),
            "<Project><PropertyGroup><MainSource>App.dpr</MainSource><Platform>Win32</Platform></PropertyGroup></Project>",
        );
        write_file(
            &root.join(name).join("App.dpr"),
            "program App; uses Main; begin end.",
        );
        write_file(
            &root.join(name).join("Main.pas"),
            "unit Main; interface uses SdkUnit; implementation procedure Run; var Value: Integer; begin Value := SdkValue; end; end.",
        );
        config.push_str(&format!(
            "[projects.\"{name}/App.dproj\"]\ninstallation='{version}'\n"
        ));
    }
    write_file(&root.join(".delphi-tools.local.toml"), &config);
    SelectionFixture {
        project_a: root.join("a/App.dproj"),
        project_b: root.join("b/App.dproj"),
        main_a: root.join("a/Main.pas"),
        directory,
    }
}

#[test]
fn select_installation_switches_the_selected_profile() {
    let fixture = selection_fixture();
    assert!(fixture.project_b.is_file());
    assert!(fixture.main_a.is_file());
    let mut server = TestServer::launch();
    server.initialize(fixture.directory.path(), Value::Null);
    let project = fixture.project_a.clone();
    let id = RequestId::from("installation-switch".to_owned());
    server.send_request(
        id.clone(),
        "pascal/selectInstallation",
        json!({
            "projectUri": uri(&project), "installationId": "37.0"
        }),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    assert_eq!(response.result.unwrap()["selectedInstallationId"], "37.0");
    server.shutdown();
}

#[test]
fn installation_selection_supports_reset_and_rejects_invalid_requests() {
    let fixture = selection_fixture();
    let mut server = TestServer::launch();
    let initialize = server.initialize(fixture.directory.path(), Value::Null);
    assert_eq!(
        initialize["capabilities"]["experimental"]["installationSelection"],
        true
    );

    let project = uri(&fixture.project_a);
    let select = RequestId::from("select-profile".to_owned());
    server.send_request(
        select.clone(),
        "pascal/selectInstallation",
        json!({
            "projectUri": project, "installationId": "37.0"
        }),
    );
    let selected = server.response(&select);
    assert!(selected.error.is_none(), "{selected:?}");
    assert_eq!(
        selected.result.as_ref().unwrap()["selectedInstallationId"],
        "37.0"
    );

    let reset = RequestId::from("reset-profile".to_owned());
    server.send_request(
        reset.clone(),
        "pascal/selectInstallation",
        json!({
            "projectUri": uri(&fixture.project_a), "installationId": null
        }),
    );
    let reset = server.response(&reset);
    assert!(reset.error.is_none(), "{reset:?}");
    assert_eq!(reset.result.unwrap()["selectedInstallationId"], "7.0");

    let invalid = RequestId::from("invalid-profile".to_owned());
    server.send_request(
        invalid.clone(),
        "pascal/selectInstallation",
        json!({
            "projectUri": uri(&fixture.project_a), "installationId": "99.0"
        }),
    );
    assert!(server.response(&invalid).error.is_some());

    let context_id = RequestId::from("selection-preserved-after-invalid-id".to_owned());
    server.send_request(
        context_id.clone(),
        "pascal/installationContext",
        json!({"projectUri": uri(&fixture.project_a)}),
    );
    assert_eq!(
        server.response(&context_id).result.unwrap()["selectedInstallationId"],
        "7.0",
        "rejecting an invalid profile must preserve the previous selection"
    );

    let omitted = RequestId::from("omitted-profile".to_owned());
    server.send_request(
        omitted.clone(),
        "pascal/selectInstallation",
        json!({
            "projectUri": uri(&fixture.project_a)
        }),
    );
    assert_eq!(server.response(&omitted).error.unwrap().code, -32602);
    server.shutdown();
}

#[test]
fn installation_context_and_project_context_report_additive_fields() {
    let fixture = selection_fixture();
    let mut server = TestServer::launch();
    server.initialize(fixture.directory.path(), Value::Null);

    let installation = RequestId::from("installation-context".to_owned());
    server.send_request(
        installation.clone(),
        "pascal/installationContext",
        json!({
            "projectUri": uri(&fixture.project_a)
        }),
    );
    let installation = server.response(&installation);
    assert!(installation.error.is_none(), "{installation:?}");
    assert_eq!(
        installation.result.as_ref().unwrap()["selectedInstallationId"],
        "7.0"
    );
    assert_eq!(
        installation.result.unwrap()["candidates"],
        json!(["37.0", "7.0"])
    );

    let context = RequestId::from("project-context-installation-fields".to_owned());
    server.send_request(
        context.clone(),
        "pascal/projectContext",
        json!({
            "textDocument": {"uri": uri(&fixture.main_a)}
        }),
    );
    let context = server.response(&context);
    assert!(context.error.is_none(), "{context:?}");
    let context = context.result.unwrap();
    assert_eq!(context["selectedInstallationId"], "7.0");
    assert_eq!(context["installationCandidates"], json!(["37.0", "7.0"]));
    assert!(context["mainSourceUri"].is_string());
    assert!(context["installationConfigUris"].is_array());
    assert!(context["pathIssues"].is_array());
    server.shutdown();
}

#[test]
fn installation_methods_reject_non_file_uris() {
    let fixture = selection_fixture();
    let mut server = TestServer::launch();
    server.initialize(fixture.directory.path(), Value::Null);
    let id = RequestId::from("unsupported-uri".to_owned());
    server.send_request(
        id.clone(),
        "pascal/installationContext",
        json!({
            "projectUri": "https://example.invalid/App.dproj"
        }),
    );
    assert!(server.response(&id).error.is_some());
    server.shutdown();
}

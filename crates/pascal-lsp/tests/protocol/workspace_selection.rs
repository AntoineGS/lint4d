use super::*;

fn ambiguous_projects() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let unit = root.join("src/Unit.pas");
    let project_a = root.join("src/A.dproj");
    let project_b = root.join("src/B.dproj");
    write_file(&unit, "unit Unit; interface implementation end.");
    let contents =
        "<Project><PropertyGroup><MainSource>Unit.pas</MainSource></PropertyGroup></Project>";
    write_file(&project_a, contents);
    write_file(&project_b, contents);
    (directory, unit, project_a, project_b)
}

fn open_automatic_unit(server: &mut TestServer, unit: &std::path::Path) {
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": uri(unit), "languageId": "pascal", "version": 1,
            "text": "unit Unit; interface implementation end."
        }}),
    );
}

fn project_context(server: &mut TestServer, unit: &std::path::Path, suffix: &str) -> Value {
    let id = RequestId::from(format!("project-prompt-context-{suffix}"));
    server.send_request(
        id.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(unit)}}),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    response.result.unwrap()
}

#[test]
fn automatic_project_ambiguity_asks_the_client_to_choose_a_project() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write_file(
        &root.join("a/App.dproj"),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup></Project>",
    );
    write_file(&root.join("a/App.dpr"), "program App; begin end.");
    write_file(
        &root.join("b/App.dproj"),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup></Project>",
    );
    write_file(&root.join("b/App.dpr"), "program App; begin end.");
    write_file(
        &root.join("src/Unit.pas"),
        "unit Unit; interface implementation end.",
    );

    // Both projects are equally near by putting a project in the unit's directory.
    let project_a = root.join("src/A.dproj");
    let project_b = root.join("src/B.dproj");
    write_file(
        &project_a,
        "<Project><PropertyGroup><MainSource>Unit.pas</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &project_b,
        "<Project><PropertyGroup><MainSource>Unit.pas</MainSource></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&root.join("src/Unit.pas")), "languageId": "pascal", "version": 1, "text": "unit Unit; interface implementation end."}}),
    );
    let request = server
        .request_with_timeout("window/showMessageRequest", Duration::from_secs(5))
        .expect("project ambiguity prompt");
    assert_eq!(request.params["type"], 3);
    let actions = request.params["actions"].as_array().unwrap();
    assert_eq!(actions.len(), 2);
    let chosen = actions[0].clone();
    let ordinary_id = RequestId::from("ordinary-while-project-prompt-pending".to_owned());
    server.send_request(
        ordinary_id.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&root.join("src/Unit.pas"))}}),
    );
    let ordinary = server.response(&ordinary_id);
    assert!(ordinary.error.is_none(), "{ordinary:?}");

    let response = Response::new_ok(request.id, chosen);
    server.send(Message::Response(response.clone()));
    server.send(Message::Response(response));

    let deadline = Instant::now() + Duration::from_secs(5);
    let expected = uri(&project_a).to_string();
    let mut selected_uri = Value::Null;
    let mut attempt = 0;
    while Instant::now() < deadline && selected_uri != expected {
        let context_id = RequestId::from(format!("automatic-project-prompt-context-{attempt}"));
        attempt += 1;
        server.send_request(
            context_id.clone(),
            "pascal/projectContext",
            json!({"textDocument": {"uri": uri(&root.join("src/Unit.pas"))}}),
        );
        let context = server.response(&context_id);
        if context.error.is_none() {
            selected_uri = context.result.unwrap()["selectedProjectUri"].clone();
        }
        if selected_uri != expected {
            thread::sleep(Duration::from_millis(10));
        }
    }
    assert_eq!(selected_uri, expected);
    server.shutdown();
}

#[test]
fn stale_automatic_answer_cannot_override_a_manual_project_choice() {
    let (directory, unit, project_a, project_b) = ambiguous_projects();
    let mut server = TestServer::launch();
    server.initialize(directory.path(), Value::Null);
    open_automatic_unit(&mut server, &unit);
    let prompt = server
        .request_with_timeout("window/showMessageRequest", Duration::from_secs(5))
        .expect("automatic project prompt");

    let manual = RequestId::from("manual-project-choice-before-prompt-answer".to_owned());
    server.send_request(
        manual.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&unit)},
            "projectUri": uri(&project_b)
        }),
    );
    let actions = prompt.params["actions"].as_array().unwrap();
    server.send(Message::Response(Response::new_ok(
        prompt.id,
        actions[0].clone(),
    )));
    let manual_response = server.response(&manual);
    assert!(manual_response.error.is_none(), "{manual_response:?}");
    let mut context = Value::Null;
    for attempt in 0..10 {
        context = project_context(&mut server, &unit, &attempt.to_string());
        if context["selectedProjectUri"] == uri(&project_b).as_str() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(context["selectedProjectUri"], uri(&project_b).as_str());
    assert_ne!(context["selectedProjectUri"], uri(&project_a).as_str());
    server.shutdown();
}

#[test]
fn removing_prompt_candidate_before_reply_does_not_select_it() {
    let (directory, unit, project_a, project_b) = ambiguous_projects();
    let mut server = TestServer::launch();
    server.initialize(directory.path(), Value::Null);
    open_automatic_unit(&mut server, &unit);
    let prompt = server
        .request_with_timeout("window/showMessageRequest", Duration::from_secs(5))
        .expect("automatic project prompt");
    fs::remove_file(&project_a).unwrap();
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&project_a), "type": 3}]}),
    );
    let fence = RequestId::from("candidate-removal-fence".to_owned());
    server.send_request(
        fence.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&unit)}}),
    );
    let _ = server.response(&fence);
    server.send(Message::Response(Response::new_ok(
        prompt.id,
        prompt.params["actions"][0].clone(),
    )));
    let mut context = Value::Null;
    for attempt in 0..10 {
        context = project_context(&mut server, &unit, &format!("removed-{attempt}"));
        if context["selectedProjectUri"] == uri(&project_b).as_str() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(context["selectedProjectUri"], uri(&project_b).as_str());
    server.shutdown();
}

#[test]
fn changed_candidate_set_rejects_old_project_prompt_answer() {
    let (directory, unit, _, _) = ambiguous_projects();
    let mut server = TestServer::launch();
    server.initialize(directory.path(), Value::Null);
    open_automatic_unit(&mut server, &unit);
    let prompt = server
        .request_with_timeout("window/showMessageRequest", Duration::from_secs(5))
        .expect("automatic project prompt");
    let added_project = directory.path().join("src/C.dproj");
    write_file(
        &added_project,
        "<Project><PropertyGroup><MainSource>Unit.pas</MainSource></PropertyGroup></Project>",
    );
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&added_project), "type": 1}]}),
    );
    let fence = RequestId::from("candidate-addition-fence".to_owned());
    server.send_request(
        fence.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&unit)}}),
    );
    let _ = server.response(&fence);
    server.send(Message::Response(Response::new_ok(
        prompt.id,
        prompt.params["actions"][0].clone(),
    )));
    let context = project_context(&mut server, &unit, "changed-candidate-set");
    assert_eq!(context["candidates"].as_array().unwrap().len(), 3);
    assert!(context["selectedProjectUri"].is_null());
    server.shutdown();
}

#[test]
fn dismissing_prompt_allows_new_document_generation_to_prompt_again() {
    let (directory, unit, _, _) = ambiguous_projects();
    let second_unit = directory.path().join("src/Second.pas");
    write_file(&second_unit, "unit Second; interface implementation end.");
    let mut server = TestServer::launch();
    server.initialize(directory.path(), Value::Null);
    open_automatic_unit(&mut server, &unit);
    let prompt = server
        .request_with_timeout("window/showMessageRequest", Duration::from_secs(5))
        .expect("first prompt");
    server.send(Message::Response(Response::new_ok(prompt.id, Value::Null)));
    open_automatic_unit(&mut server, &second_unit);
    let next = server
        .request_with_timeout("window/showMessageRequest", Duration::from_secs(5))
        .expect("new document generation should prompt again");
    assert_eq!(next.params["actions"].as_array().unwrap().len(), 2);
    server.send(Message::Response(Response::new_ok(next.id, Value::Null)));
    server.shutdown();
}

#[test]
fn arbitrary_prompt_title_is_not_interpreted_as_a_project_path() {
    let (directory, unit, _, _) = ambiguous_projects();
    let mut server = TestServer::launch();
    server.initialize(directory.path(), Value::Null);
    open_automatic_unit(&mut server, &unit);
    let prompt = server
        .request_with_timeout("window/showMessageRequest", Duration::from_secs(5))
        .expect("automatic project prompt");
    server.send(Message::Response(Response::new_ok(
        prompt.id,
        json!({"title": "../../outside/Injected.dproj"}),
    )));
    let context = project_context(&mut server, &unit, "untrusted-title");
    assert!(context["selectedProjectUri"].is_null());
    server.shutdown();
}

#[test]
fn project_prompt_finishes_before_installation_prompt_starts() {
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
        write_file(
            &ide.join("EnvOptions.proj"),
            "<Project><PropertyGroup><Win32LibraryPath>$(BDS)\\source</Win32LibraryPath></PropertyGroup></Project>",
        );
        write_file(
            &sdk.join("source/SdkUnit.pas"),
            "unit SdkUnit; interface implementation end.",
        );
        config.push_str(&format!(
            "[installations.\"{version}\".properties]\nBDS='{}'\nAPPDATA='{}'\nEnvironmentSettings='{}/EnvOptions.proj'\n",
            sdk.display(), ide.display(), ide.display(),
        ));
    }
    let unit = root.join("src/Unit.pas");
    write_file(&unit, "unit Unit; interface implementation end.");
    for project in ["A.dproj", "B.dproj"] {
        write_file(
            &root.join("src").join(project),
            "<Project><PropertyGroup><MainSource>Unit.pas</MainSource></PropertyGroup></Project>",
        );
    }
    write_file(&root.join(".delphi-tools.local.toml"), &config);

    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);
    open_automatic_unit(&mut server, &unit);
    let project_prompt = server
        .request_with_timeout("window/showMessageRequest", Duration::from_secs(5))
        .expect("project prompt first");
    server.send(Message::Response(Response::new_ok(
        project_prompt.id,
        project_prompt.params["actions"][0].clone(),
    )));
    let installation_prompt = server
        .request_with_timeout("window/showMessageRequest", Duration::from_secs(5))
        .expect("installation prompt after project selection");
    assert_eq!(
        installation_prompt.params["message"],
        "Choose the Delphi installation to use"
    );
    assert_eq!(
        installation_prompt.params["actions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let installation_action = installation_prompt.params["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|action| action["title"] == "37.0")
        .unwrap()
        .clone();
    server.send(Message::Response(Response::new_ok(
        installation_prompt.id,
        installation_action,
    )));
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut installation = Value::Null;
    let mut attempt = 0;
    while Instant::now() < deadline && installation != "37.0" {
        let id = RequestId::from(format!("installation-after-prompt-{attempt}"));
        attempt += 1;
        server.send_request(
            id.clone(),
            "pascal/installationContext",
            json!({"projectUri": uri(&root.join("src/A.dproj"))}),
        );
        let response = server.response(&id);
        if response.error.is_none() {
            installation = response.result.unwrap()["selectedInstallationId"].clone();
        }
        if installation != "37.0" {
            thread::sleep(Duration::from_millis(10));
        }
    }
    assert_eq!(installation, "37.0");
    server.shutdown();
}

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
            "[installations.\"{version}\".properties]\nBDS='{}'\nAPPDATA='{}'\nEnvironmentSettings='{}/EnvOptions.proj'\n",
            sdk.display(),
            ide.display(),
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
fn reset_installation_response_does_not_reuse_cached_prior_profile() {
    let fixture = selection_fixture();
    let config_path = fixture.directory.path().join(".delphi-tools.local.toml");
    let config = fs::read_to_string(&config_path)
        .expect("read fixture configuration")
        .replace("[projects.\"a/App.dproj\"]\ninstallation='7.0'\n", "")
        .replace("[projects.\"b/App.dproj\"]\ninstallation='37.0'\n", "");
    write_file(&config_path, &config);

    let mut server = TestServer::launch();
    server.initialize(fixture.directory.path(), Value::Null);
    let selected = RequestId::from("cached-profile-seed".to_owned());
    server.send_request(
        selected.clone(),
        "pascal/selectInstallation",
        json!({"projectUri": uri(&fixture.project_a), "installationId": "37.0"}),
    );
    assert!(server.response(&selected).error.is_none());

    let project_context = RequestId::from("cache-selected-profile-context".to_owned());
    server.send_request(
        project_context.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&fixture.main_a)}}),
    );
    assert!(server.response(&project_context).error.is_none());

    let reset = RequestId::from("clear-session-profile".to_owned());
    server.send_request(
        reset.clone(),
        "pascal/selectInstallation",
        json!({"projectUri": uri(&fixture.project_a), "installationId": null}),
    );
    let reset = server.response(&reset);
    assert!(reset.error.is_none(), "{reset:?}");
    assert_ne!(
        reset.result.unwrap()["selectedInstallationId"],
        "37.0",
        "reset response must describe post-reset automatic selection, not cached session data"
    );
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
    let installation_config_uris = context["installationConfigUris"]
        .as_array()
        .expect("installation config URI array");
    assert!(
        installation_config_uris
            .iter()
            .any(|uri| { uri.as_str().is_some_and(|uri| uri.ends_with("/rsvars.bat")) })
    );
    assert!(installation_config_uris.iter().any(|uri| {
        uri.as_str()
            .is_some_and(|uri| uri.ends_with("/EnvOptions.proj"))
    }));
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

#[cfg(feature = "test-support")]
#[test]
fn held_project_operation_worker_does_not_block_ordinary_requests() {
    let fixture = selection_fixture();
    let environment = tempfile::tempdir().unwrap();
    let (mut server, barrier) = TestServer::launch_with_project_operation_barrier(environment);
    server.initialize(fixture.directory.path(), Value::Null);

    let select = RequestId::from("held-project-operation".to_owned());
    server.send_request(
        select.clone(),
        "pascal/selectInstallation",
        json!({"projectUri": uri(&fixture.project_a), "installationId": "37.0"}),
    );
    wait_for_path(&barrier.entered);

    let ordinary = RequestId::from("ordinary-request-during-project-worker".to_owned());
    server.send_request(
        ordinary.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&fixture.main_a)}}),
    );
    let ordinary_response = server.response(&ordinary);
    assert!(ordinary_response.error.is_none(), "{ordinary_response:?}");

    fs::write(&barrier.release, b"release").expect("release project worker");
    let selected = server.response(&select);
    assert!(selected.error.is_none(), "{selected:?}");
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn select_project_runs_on_the_bounded_project_worker() {
    let fixture = selection_fixture();
    let environment = tempfile::tempdir().unwrap();
    let (mut server, barrier) = TestServer::launch_with_project_operation_barrier(environment);
    server.initialize(fixture.directory.path(), Value::Null);

    let select = RequestId::from("select-project-worker-route".to_owned());
    server.send_request(
        select.clone(),
        "pascal/selectProject",
        json!({
            "textDocument": {"uri": uri(&fixture.main_a)},
            "projectUri": uri(&fixture.project_a)
        }),
    );
    wait_for_path(&barrier.entered);

    let ordinary = RequestId::from("ordinary-request-during-select-project".to_owned());
    server.send_request(
        ordinary.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&fixture.main_a)}}),
    );
    let ordinary_response = server.response(&ordinary);
    assert!(ordinary_response.error.is_none(), "{ordinary_response:?}");

    fs::write(&barrier.release, b"release").expect("release project operation worker");
    let selected = server.response(&select);
    assert!(selected.error.is_none(), "{selected:?}");
    assert_eq!(
        selected.result.unwrap()["selectedProjectUri"],
        uri(&fixture.project_a).as_str()
    );
    let context = RequestId::from("project-context-after-worker-selection".to_owned());
    server.send_request(
        context.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&fixture.main_a)}}),
    );
    let context = server.response(&context);
    assert!(context.error.is_none(), "{context:?}");
    assert_eq!(
        context.result.unwrap()["selectedProjectUri"],
        uri(&fixture.project_a).as_str()
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn queued_project_selection_cancelled_before_worker_start_is_not_committed() {
    let fixture = selection_fixture();
    let environment = tempfile::tempdir().unwrap();
    let (mut server, barrier) = TestServer::launch_with_project_operation_barrier(environment);
    server.initialize(fixture.directory.path(), Value::Null);

    let request_id = |name: &str| RequestId::from(name.to_owned());
    for (name, profile) in [("project-worker-a", "37.0"), ("project-worker-b", "37.0")] {
        server.send_request(
            request_id(name),
            "pascal/selectInstallation",
            json!({"projectUri": uri(&fixture.project_a), "installationId": profile}),
        );
    }
    let deadline = Instant::now() + IO_TIMEOUT;
    while Instant::now() < deadline && fs::read(&barrier.entered).map_or(0, |bytes| bytes.len()) < 2
    {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        fs::read(&barrier.entered).map_or(0, |bytes| bytes.len()) >= 2,
        "both project operation workers should occupy bounded worker slots"
    );

    let queued = request_id("project-worker-queued-cancel");
    server.send_request(
        queued.clone(),
        "pascal/selectInstallation",
        json!({"projectUri": uri(&fixture.project_b), "installationId": "7.0"}),
    );
    server.send_notification("$/cancelRequest", json!({"id": queued}));
    assert_eq!(server.response(&queued).error.unwrap().code, -32800);

    fs::write(&barrier.release, b"release").expect("release project workers");
    for name in ["project-worker-a", "project-worker-b"] {
        let _ = server.response(&request_id(name));
    }
    let context = RequestId::from("queued-cancel-project-b-context".to_owned());
    server.send_request(
        context.clone(),
        "pascal/installationContext",
        json!({"projectUri": uri(&fixture.project_b)}),
    );
    assert_eq!(
        server.response(&context).result.unwrap()["selectedInstallationId"],
        "37.0",
        "the canceled queued selection must not change project B"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn selection_worker_result_is_rejected_after_installation_profile_change() {
    let fixture = selection_fixture();
    let environment = tempfile::tempdir().unwrap();
    let (mut server, barrier) = TestServer::launch_with_project_operation_barrier(environment);
    server.initialize(fixture.directory.path(), Value::Null);

    let select = RequestId::from("selection-stale-after-profile-change".to_owned());
    server.send_request(
        select.clone(),
        "pascal/selectInstallation",
        json!({"projectUri": uri(&fixture.project_a), "installationId": "37.0"}),
    );
    wait_for_path(&barrier.entered);

    let local_config = fixture.directory.path().join(".delphi-tools.local.toml");
    let config = fs::read_to_string(&local_config)
        .expect("read local config")
        .replace("installations.\"37.0\"", "installations.\"38.0\"");
    write_file(&local_config, &config);
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&local_config), "type": 2}]}),
    );

    let ordinary = RequestId::from("profile-change-ordering-fence".to_owned());
    server.send_request(
        ordinary.clone(),
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&fixture.main_a)}}),
    );
    let ordinary_response = server.response(&ordinary);
    assert!(ordinary_response.error.is_none(), "{ordinary_response:?}");

    fs::write(&barrier.release, b"release").expect("release project worker");
    let stale = server.response(&select);
    assert!(
        stale.error.is_some(),
        "a selection prepared against the prior profile generation must not commit: {stale:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn selection_result_is_rejected_for_disk_profile_change_without_notification() {
    let fixture = selection_fixture();
    let environment = tempfile::tempdir().unwrap();
    let (mut server, barrier) =
        TestServer::launch_with_project_operation_prepared_barrier(environment);
    server.initialize(fixture.directory.path(), Value::Null);

    let selection = RequestId::from("selection-unnotified-profile-change".to_owned());
    server.send_request(
        selection.clone(),
        "pascal/selectInstallation",
        json!({"projectUri": uri(&fixture.project_a), "installationId": "37.0"}),
    );
    wait_for_path(&barrier.entered);

    let config_path = fixture.directory.path().join(".delphi-tools.local.toml");
    let original = fs::read_to_string(&config_path).expect("read captured profile config");
    let replacement = original.replace(
        &fixture
            .directory
            .path()
            .join("sdk/37.0")
            .display()
            .to_string(),
        &fixture
            .directory
            .path()
            .join("sdk/7.0")
            .display()
            .to_string(),
    );
    assert_ne!(original, replacement, "fixture profile must be changed");
    write_file(&config_path, &replacement);
    // Deliberately do not send didChangeWatchedFiles: delivery must verify the
    // worker's captured on-disk read set rather than trusting generations only.
    fs::write(&barrier.release, b"release").expect("release prepared worker");

    let response = server.response(&selection);
    assert!(
        response.error.is_some(),
        "profile/config changed after preparation without an LSP notification: {response:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn selection_rejects_configuration_changed_before_worker_discovery() {
    let fixture = selection_fixture();
    let environment = tempfile::tempdir().unwrap();
    let (mut server, barrier) = TestServer::launch_with_project_operation_barrier(environment);
    server.initialize(fixture.directory.path(), Value::Null);

    let selection = RequestId::from("selection-profile-change-before-discovery".to_owned());
    server.send_request(
        selection.clone(),
        "pascal/selectInstallation",
        json!({"projectUri": uri(&fixture.project_a), "installationId": "37.0"}),
    );
    wait_for_path(&barrier.entered);

    // The selection choice was cached from this configuration before the
    // worker began discovery. It must not be made to look fresh by stamping
    // only the edited bytes after discovery has completed.
    let config_path = fixture.directory.path().join(".delphi-tools.local.toml");
    let original = fs::read_to_string(&config_path).expect("read captured profile config");
    let replacement = original.replace(
        &fixture
            .directory
            .path()
            .join("sdk/37.0")
            .display()
            .to_string(),
        &fixture
            .directory
            .path()
            .join("sdk/7.0")
            .display()
            .to_string(),
    );
    assert_ne!(original, replacement);
    write_file(&config_path, &replacement);
    fs::write(&barrier.release, b"release").expect("release project worker");

    let response = server.response(&selection);
    assert!(
        response.error.is_some(),
        "selection used configuration bytes from before the worker's read-set baseline: {response:?}"
    );
    server.shutdown();
}

#[test]
fn installation_context_reconciles_cached_config_before_request_baseline() {
    let fixture = selection_fixture();
    let config_path = fixture.directory.path().join(".delphi-tools.local.toml");
    let original = fs::read_to_string(&config_path).expect("read fixture configuration");
    let changed = original.replace(
        "[projects.\"a/App.dproj\"]\ninstallation='7.0'",
        "[projects.\"a/App.dproj\"]\ninstallation='37.0'",
    );
    assert_ne!(original, changed);

    let mut server = TestServer::launch();
    server.initialize(fixture.directory.path(), Value::Null);
    let priming = RequestId::from("prime-cached-seven-profile".to_owned());
    server.send_request(
        priming.clone(),
        "pascal/installationContext",
        json!({"projectUri": uri(&fixture.project_a)}),
    );
    assert_eq!(
        server.response(&priming).result.unwrap()["selectedInstallationId"],
        "7.0"
    );
    // Prime the shared OverrideSession with 7.0 before the silent edit; the
    // next request's raw-stamp baseline will see the changed bytes.
    write_file(&config_path, &changed);

    let request = RequestId::from("fresh-installation-after-silent-edit".to_owned());
    server.send_request(
        request.clone(),
        "pascal/installationContext",
        json!({"projectUri": uri(&fixture.project_a)}),
    );
    let response = server.response(&request);
    let result = response.result.as_ref().cloned().unwrap_or(Value::Null);
    assert!(
        response.error.is_some() || result["selectedInstallationId"] == "37.0",
        "operation used cached configuration from before its raw-stamp baseline: {response:?}"
    );
    server.shutdown();
}

#[test]
fn installation_context_reconciles_repaired_cached_config_before_baseline() {
    let fixture = selection_fixture();
    let config_path = fixture.directory.path().join(".delphi-tools.local.toml");
    write_file(&config_path, "[projects\n");

    let mut server = TestServer::launch();
    server.initialize(fixture.directory.path(), Value::Null);
    let malformed_request = RequestId::from("observe-malformed-cached-config".to_owned());
    server.send_request(
        malformed_request.clone(),
        "pascal/installationContext",
        json!({"projectUri": uri(&fixture.project_a)}),
    );
    let malformed_response = server.response(&malformed_request);
    assert!(
        malformed_response
            .error
            .as_ref()
            .is_some_and(|error| error.message.contains("failed to parse")),
        "malformed configuration should retain its existing parse diagnostic: {malformed_response:?}"
    );
    // Initialization captures the malformed file as an error. Repair it
    // without a file notification before this request's fresh disk baseline.
    write_file(
        &config_path,
        "[installations.\"37.0\".properties]\nBDS='/fake/37'\n[projects.\"a/App.dproj\"]\ninstallation='37.0'\n",
    );

    let request = RequestId::from("repaired-config-after-silent-edit".to_owned());
    server.send_request(
        request.clone(),
        "pascal/installationContext",
        json!({"projectUri": uri(&fixture.project_a)}),
    );
    let response = server.response(&request);
    let result = response.result.as_ref().cloned().unwrap_or(Value::Null);
    assert!(
        result["selectedInstallationId"] == "37.0"
            || response
                .error
                .as_ref()
                .is_some_and(|error| error.message.contains("retry the request")),
        "operation consumed a cached parse error after the config was repaired: {response:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn explicit_project_configuration_is_revalidated_after_preparation() {
    let fixture = selection_fixture();
    let explicit_directory = fixture.directory.path().join("explicit-project");
    let explicit_project = explicit_directory.join("Configured.dproj");
    write_file(
        &explicit_project,
        "<Project><PropertyGroup><MainSource>Configured.dpr</MainSource></PropertyGroup></Project>",
    );
    write_file(
        &explicit_directory.join("Configured.dpr"),
        "program Configured; begin end.",
    );
    let local_config = explicit_directory.join(".delphi-tools.local.toml");
    write_file(
        &local_config,
        "[installations.\"88.0\".properties]\nBDS='/fake/88'\n",
    );

    let environment = tempfile::tempdir().unwrap();
    let (mut server, barrier) =
        TestServer::launch_with_project_operation_prepared_barrier(environment);
    server.initialize(
        fixture.directory.path(),
        json!({"pascalLsp": {"projectFile": explicit_project.display().to_string()}}),
    );
    let request = RequestId::from("explicit-project-config-race".to_owned());
    server.send_request(
        request.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&fixture.main_a)}}),
    );
    wait_for_path(&barrier.entered);
    write_file(
        &local_config,
        "[installations.\"77.0\".properties]\nBDS='/fake/77'\n",
    );
    fs::write(&barrier.release, b"release").expect("release prepared worker");

    let response = server.response(&request);
    assert!(
        response.error.is_some(),
        "explicit project's untracked local configuration edit must stale the result: {response:?}"
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn compiled_view_response_is_stale_after_installation_switch() {
    let fixture = selection_fixture();
    let root = fixture.directory.path();
    let library = root.join("a/lib");
    fs::create_dir_all(&library).expect("create compiled library directory");
    let dcu = library.join("Lint4dFixture.Classes.dcu");
    let dcu_fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../lint4d/tests/fixtures/dcu/d13_win64/Win64/Debug/Lint4dFixture.Classes.dcu");
    fs::copy(&dcu_fixture, &dcu).expect("copy real D13 DCU fixture");
    let source = "unit Main;\ninterface\nuses Lint4dFixture.Classes;\ntype TAlias = TSimpleClass;\nimplementation\nprocedure Check(Value: TSimpleClass);\nbegin\n  Value.GetName;\nend;\nend.\n";
    write_file(&fixture.main_a, source);
    write_file(
        &fixture.project_a,
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource><Platform>Win32</Platform><DCC_UnitSearchPath>lib</DCC_UnitSearchPath></PropertyGroup></Project>",
    );
    let environment = tempfile::tempdir().unwrap();
    let (mut server, barrier) = TestServer::launch_with_compiled_content_barrier(environment);
    server.initialize(root, Value::Null);

    let definition = RequestId::from("compiled-view-definition-before-switch".to_owned());
    server.send_request(
        definition.clone(),
        "textDocument/definition",
        json!({
            "textDocument": {"uri": uri(&fixture.main_a)},
            "position": {"line": 3, "character": 16}
        }),
    );
    let definition = server.response(&definition);
    assert!(definition.error.is_none(), "{definition:?}");
    let compiled_uri = definition.result.unwrap()[0]["uri"]
        .as_str()
        .expect("compiled URI")
        .to_owned();
    assert!(compiled_uri.starts_with("lint4d-dcu://"), "{compiled_uri}");

    let content = RequestId::from("compiled-view-content-before-switch".to_owned());
    server.send_request(
        content.clone(),
        "textDocument/content",
        json!({"textDocument": {"uri": compiled_uri}}),
    );
    wait_for_path(&barrier.entered);

    let select = RequestId::from("compiled-view-installation-switch".to_owned());
    server.send_request(
        select.clone(),
        "pascal/selectInstallation",
        json!({"projectUri": uri(&fixture.project_a), "installationId": "37.0"}),
    );
    let selected = server.response(&select);
    assert!(selected.error.is_none(), "{selected:?}");

    fs::write(&barrier.release, b"release").expect("release compiled-content worker");
    assert!(
        server.response(&content).error.is_some(),
        "compiled content captured before an installation switch must be rejected"
    );
    server.shutdown();
}

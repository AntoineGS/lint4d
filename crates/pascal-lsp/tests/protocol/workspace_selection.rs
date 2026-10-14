use super::workspace_selection_support::{
    ambiguous_projects, open_automatic_unit, project_context, project_context_retry_stale,
    selection_fixture,
};
use super::*;

#[test]
fn list_projects_is_on_demand_and_advertised() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let first = root.join("apps/One/App.dproj");
    let second = root.join("apps/Two/App.dproj");
    let anchor = root.join("README.md");
    write_file(&first, "not parsed");
    write_file(&second, "not parsed");
    write_file(&anchor, "catalogue anchor");
    let local_config = root.join(".delphi-tools.local.toml");
    write_file(&local_config, "[properties]\nBDS = '/sdk'\n");
    let mut server = TestServer::launch();
    let capabilities = server.initialize(root, Value::Null);
    assert_eq!(
        capabilities["capabilities"]["experimental"]["projectCatalogue"],
        true
    );
    let id = RequestId::from("list-projects".to_owned());
    #[cfg(target_os = "linux")]
    let mut result = Value::Null;
    #[cfg(target_os = "linux")]
    let (opened, config_opened) = {
        let opened = observed_open(&first, || {
            server.send_request(
                id.clone(),
                "pascal/listProjects",
                json!({"textDocument": {"uri": uri(&anchor)}}),
            );
            let response = server.response(&id);
            assert!(response.error.is_none(), "{response:?}");
            result = response.result.unwrap();
        });
        let config_opened = observed_open(&local_config, || {
            let id = RequestId::from("list-projects-no-config-read".to_owned());
            server.send_request(
                id.clone(),
                "pascal/listProjects",
                json!({"textDocument": {"uri": uri(&anchor)}}),
            );
            let response = server.response(&id);
            assert!(response.error.is_none(), "{response:?}");
            result = response.result.unwrap();
        });
        (opened, config_opened)
    };
    #[cfg(not(target_os = "linux"))]
    let result = {
        server.send_request(
            id.clone(),
            "pascal/listProjects",
            json!({"textDocument": {"uri": uri(&root.join("README.md"))}}),
        );
        let response = server.response(&id);
        assert!(response.error.is_none(), "{response:?}");
        response.result.unwrap()
    };
    #[cfg(target_os = "linux")]
    assert!(!opened, "enumeration must not open .dproj contents");
    #[cfg(target_os = "linux")]
    assert!(
        !config_opened,
        "listing admission must not read project configuration"
    );
    assert_eq!(result["complete"], true);
    assert_eq!(result["projects"][0]["label"], "apps/One/App.dproj");
    assert_eq!(result["projects"][1]["label"], "apps/Two/App.dproj");
    server.shutdown();
}

#[test]
fn list_projects_rejects_an_anchor_outside_the_workspace_root() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let outside = directory.path().join("root-sibling");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let anchor = outside.join("README.md");
    write_file(&anchor, "outside");
    let mut server = TestServer::launch();
    server.initialize(&root, Value::Null);
    let id = RequestId::from("list-projects-outside-root".to_owned());
    server.send_request(
        id.clone(),
        "pascal/listProjects",
        json!({"textDocument": {"uri": uri(&anchor)}}),
    );
    let response = server.response(&id);
    assert!(response.error.is_some());
    server.shutdown();
}

#[test]
fn project_file_is_a_protocol_only_project_anchor() {
    let (directory, unit, project_a, project_b) = ambiguous_projects();
    let mut server = TestServer::launch();
    server.initialize(directory.path(), Value::Null);
    let id = RequestId::from("select-project-file-anchor".to_owned());
    server.send_request(
        id.clone(),
        "pascal/selectProject",
        json!({"textDocument": {"uri": uri(&project_a)}, "projectUri": uri(&project_a)}),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    assert_eq!(
        response.result.unwrap()["selectedProjectUri"],
        uri(&project_a).as_str()
    );
    assert_eq!(
        project_context(&mut server, &unit, "dproj-anchor")["selectedProjectUri"],
        uri(&project_a).as_str()
    );
    server.shutdown();
    let _ = project_b;
}

#[test]
fn deleted_catalogue_choice_does_not_replace_live_selection() {
    let (directory, unit, project_a, project_b) = ambiguous_projects();
    let mut server = TestServer::launch();
    server.initialize(directory.path(), Value::Null);
    let browse = RequestId::from("browse-before-delete".to_owned());
    server.send_request(
        browse.clone(),
        "pascal/listProjects",
        json!({"textDocument": {"uri": uri(&unit)}}),
    );
    let catalogue = server.response(&browse).result.unwrap();
    let stale_project = catalogue["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["projectUri"] == uri(&project_b).as_str())
        .unwrap()["projectUri"]
        .clone();
    let select = RequestId::from("select-live-a".to_owned());
    server.send_request(
        select.clone(),
        "pascal/selectProject",
        json!({"textDocument": {"uri": uri(&unit)}, "projectUri": uri(&project_a)}),
    );
    assert!(server.response(&select).error.is_none());
    fs::remove_file(&project_b).unwrap();
    let stale = RequestId::from("select-deleted-candidate".to_owned());
    server.send_request(
        stale.clone(),
        "pascal/selectProject",
        json!({"textDocument": {"uri": uri(&unit)}, "projectUri": stale_project}),
    );
    assert!(server.response(&stale).error.is_some());
    let context = project_context(&mut server, &unit, "stale-project");
    assert_eq!(context["selectedProjectUri"], uri(&project_a).as_str());
    server.shutdown();
}

#[test]
fn project_context_without_main_source_remains_inspectable() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("App.dproj");
    write_file(
        &project,
        "<Project><PropertyGroup><ProjectVersion>37.0</ProjectVersion></PropertyGroup></Project>",
    );
    let mut server = TestServer::launch();
    server.initialize(directory.path(), Value::Null);
    let id = RequestId::from("context-without-main-source".to_owned());
    server.send_request(
        id.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&project)}}),
    );
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    let result = response.result.unwrap();
    assert_eq!(result["selectedProjectUri"], uri(&project).as_str());
    assert!(result["mainSourceUri"].is_null());
    assert!(
        result["pathIssues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| {
                issue["kind"] == "MissingMainSource" && issue["property"] == "MainSource"
            })
    );
    assert!(result["warnings"].as_array().is_some());
    server.shutdown();
}

fn nested_scope_projects(
    child_has_candidates: bool,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let parent_unit = root.join("Unit.pas");
    write_file(&parent_unit, "unit Unit; interface implementation end.");
    let parent_project_a = root.join("ParentA.dproj");
    let parent_project_b = root.join("ParentB.dproj");
    let project =
        "<Project><PropertyGroup><MainSource>Unit.pas</MainSource></PropertyGroup></Project>";
    write_file(&parent_project_a, project);
    write_file(&parent_project_b, project);
    let child_root = root.join("child");
    let child_unit = child_root.join("Unit.pas");
    write_file(&child_unit, "unit Unit; interface implementation end.");
    if child_has_candidates {
        let child_project =
            "<Project><PropertyGroup><MainSource>Unit.pas</MainSource></PropertyGroup></Project>";
        write_file(&child_root.join("ChildA.dproj"), child_project);
        write_file(&child_root.join("ChildB.dproj"), child_project);
    }
    (
        directory,
        parent_unit,
        parent_project_a,
        parent_project_b,
        child_unit,
        child_root,
    )
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
fn close_and_reopen_rejects_the_old_prompt_document_identity() {
    let (directory, unit, _, _) = ambiguous_projects();
    let mut server = TestServer::launch();
    server.initialize(directory.path(), Value::Null);
    open_automatic_unit(&mut server, &unit);
    let old_prompt = server
        .request_with_timeout("window/showMessageRequest", Duration::from_secs(5))
        .expect("initial automatic project prompt");
    server.send_notification(
        "textDocument/didClose",
        json!({"textDocument": {"uri": uri(&unit)}}),
    );
    open_automatic_unit(&mut server, &unit);
    let replacement_prompt = server
        .request_with_timeout("window/showMessageRequest", Duration::from_secs(5))
        .expect("new document identity should be prompted independently");

    server.send(Message::Response(Response::new_ok(
        old_prompt.id,
        old_prompt.params["actions"][0].clone(),
    )));
    let context = project_context_retry_stale(&mut server, &unit, "reopened-old-reply");
    assert_eq!(context["selectedProjectUri"], Value::Null);

    server.send(Message::Response(Response::new_ok(
        replacement_prompt.id,
        Value::Null,
    )));
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
fn invalid_installation_selection_does_not_trigger_automatic_install_prompt() {
    let fixture = selection_fixture();
    let config_path = fixture.directory.path().join(".delphi-tools.local.toml");
    let config = fs::read_to_string(&config_path)
        .unwrap()
        .replace("installation='7.0'", "installation='99.0'");
    write_file(&config_path, &config);

    let mut server = TestServer::launch();
    server.initialize(fixture.directory.path(), Value::Null);
    open_automatic_unit(&mut server, &fixture.main_a);
    assert!(
        server
            .request_with_timeout("window/showMessageRequest", Duration::from_millis(250))
            .is_none(),
        "an invalid persisted selection must not be offered as NeedsChoice"
    );
    server.shutdown();
}

#[test]
fn child_with_own_project_scope_does_not_fence_parent_prompt() {
    let (directory, parent_unit, parent_a, _, child_unit, _) = nested_scope_projects(true);
    let mut server = TestServer::launch();
    server.initialize(directory.path(), Value::Null);
    open_automatic_unit(&mut server, &parent_unit);
    let parent_prompt = server
        .request_with_timeout("window/showMessageRequest", Duration::from_secs(5))
        .expect("parent ambiguity prompt");
    open_automatic_unit(&mut server, &child_unit);

    let manual = RequestId::from("select-child-own-scope".to_owned());
    server.send_request(
        manual.clone(),
        "pascal/selectProject",
        json!({"textDocument": {"uri": uri(&child_unit)}, "projectUri": uri(&directory.path().join("child/ChildB.dproj"))}),
    );
    let response = server.response(&manual);
    assert!(response.error.is_none(), "{response:?}");
    let child_context = project_context_retry_stale(&mut server, &child_unit, "child-own-scope");
    assert_eq!(
        child_context["selectedProjectUri"],
        uri(&directory.path().join("child/ChildB.dproj")).as_str()
    );

    let parent_action = parent_prompt.params["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|action| action["title"] == "ParentA.dproj")
        .unwrap()
        .clone();
    server.send(Message::Response(Response::new_ok(
        parent_prompt.id,
        parent_action,
    )));
    let mut parent_context = Value::Null;
    for attempt in 0..10 {
        parent_context = project_context_retry_stale(
            &mut server,
            &parent_unit,
            &format!("parent-after-child-own-{attempt}"),
        );
        if parent_context["selectedProjectUri"] == uri(&parent_a).as_str() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        parent_context["selectedProjectUri"],
        uri(&parent_a).as_str()
    );
    server.shutdown();
}

#[test]
fn child_inheriting_parent_scope_is_fenced_by_parent_manual_choice() {
    let (directory, parent_unit, parent_a, parent_b, child_unit, _) = nested_scope_projects(false);
    let mut server = TestServer::launch();
    server.initialize(directory.path(), Value::Null);
    open_automatic_unit(&mut server, &parent_unit);
    let parent_prompt = server
        .request_with_timeout("window/showMessageRequest", Duration::from_secs(5))
        .expect("parent ambiguity prompt");
    open_automatic_unit(&mut server, &child_unit);

    let manual = RequestId::from("select-inherited-parent-scope".to_owned());
    server.send_request(
        manual.clone(),
        "pascal/selectProject",
        json!({"textDocument": {"uri": uri(&child_unit)}, "projectUri": uri(&parent_b)}),
    );
    let response = server.response(&manual);
    assert!(response.error.is_none(), "{response:?}");
    let parent_action = parent_prompt.params["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|action| action["title"] == "ParentA.dproj")
        .unwrap()
        .clone();
    server.send(Message::Response(Response::new_ok(
        parent_prompt.id,
        parent_action,
    )));

    let parent_context =
        project_context_retry_stale(&mut server, &parent_unit, "inherited-parent-wins");
    assert_eq!(
        parent_context["selectedProjectUri"],
        uri(&parent_b).as_str()
    );
    assert_ne!(
        parent_context["selectedProjectUri"],
        uri(&parent_a).as_str()
    );
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
fn deleting_the_selected_profile_does_not_fall_back_to_another_profile() {
    let fixture = selection_fixture();
    let config = fixture.directory.path().join(".delphi-tools.local.toml");
    let mut server = TestServer::launch();
    server.initialize(fixture.directory.path(), Value::Null);

    let select = RequestId::from("choose-profile-before-removal".to_owned());
    server.send_request(
        select.clone(),
        "pascal/selectInstallation",
        json!({"projectUri": uri(&fixture.project_a), "installationId": "37.0"}),
    );
    let selected = server.response(&select);
    assert!(selected.error.is_none(), "{selected:?}");
    assert_eq!(selected.result.unwrap()["selectedInstallationId"], "37.0");

    let before = fs::read_to_string(&config).expect("read local configuration");
    let without_selected = before.replace(
        "[installations.\"37.0\".properties]\nBDS=",
        "[installations.\"retired\".properties]\nBDS=",
    );
    assert_ne!(before, without_selected);
    write_file(&config, &without_selected);
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&config), "type": 2}]}),
    );

    let query = RequestId::from("query-removed-selected-profile".to_owned());
    server.send_request(
        query.clone(),
        "pascal/installationContext",
        json!({"projectUri": uri(&fixture.project_a)}),
    );
    let response = server.response(&query);
    assert!(
        response.error.is_some()
            || response.result.as_ref().is_some_and(|result| {
                result["selectedInstallationId"].is_null() && result["selectionMode"] == "invalid"
            }),
        "a removed explicit profile must stay invalid, not select the remaining/newest profile: {response:?}"
    );
    server.shutdown();
}

#[test]
fn changing_profile_a_preserves_profile_b_context() {
    let fixture = selection_fixture();
    let env_options_a = fixture.directory.path().join("ide/7.0/EnvOptions.proj");
    let mut server = TestServer::launch();
    server.initialize(fixture.directory.path(), Value::Null);

    let query_a = RequestId::from("profile-a-before-change".to_owned());
    server.send_request(
        query_a.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&fixture.main_a)}}),
    );
    let context_a = server.response(&query_a);
    assert!(context_a.error.is_none(), "{context_a:?}");

    let query_b = RequestId::from("profile-b-before-change".to_owned());
    server.send_request(
        query_b.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&fixture.directory.path().join("b/Main.pas"))}}),
    );
    let context_b = server.response(&query_b);
    assert!(context_b.error.is_none(), "{context_b:?}");
    let before_b = context_b.result.expect("project B context");
    assert_eq!(before_b["selectedInstallationId"], "37.0");

    write_file(
        &env_options_a,
        "<Project><PropertyGroup><Win32LibraryPath>$(BDS)\\replacement</Win32LibraryPath></PropertyGroup></Project>",
    );
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&env_options_a), "type": 2}]}),
    );

    let after = RequestId::from("profile-b-after-a-change".to_owned());
    server.send_request(
        after.clone(),
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&fixture.directory.path().join("b/Main.pas"))}}),
    );
    let after = server.response(&after);
    assert!(after.error.is_none(), "{after:?}");
    let after_b = after.result.expect("project B context after A change");
    assert_eq!(after_b["selectedInstallationId"], "37.0");
    assert_eq!(
        after_b["selectedProjectUri"],
        before_b["selectedProjectUri"]
    );
    assert!(
        after_b["installationConfigUris"]
            .as_array()
            .is_some_and(|uris| uris.iter().any(|uri| uri
                .as_str()
                .is_some_and(|uri| uri.ends_with("/ide/37.0/EnvOptions.proj")))),
        "profile B must retain its captured IDE layer: {after_b:?}"
    );
    server.shutdown();
}

#[test]
fn env_options_provider_changes_refresh_with_and_without_watcher() {
    for send_watcher in [true, false] {
        let fixture = selection_fixture();
        let env_options = fixture.directory.path().join("ide/7.0/EnvOptions.proj");
        let alternate = fixture.directory.path().join("sdk/7.0/alternate");
        let source = "unit Main;\ninterface\nuses SdkUnit;\nimplementation\nend.\n";
        write_file(&fixture.main_a, source);
        write_file(
            &fixture.project_a,
            "<Project><PropertyGroup><MainSource>App.dpr</MainSource><Config>Debug</Config><Platform>Win32</Platform></PropertyGroup></Project>",
        );
        write_file(
            &env_options,
            "<Project><PropertyGroup Condition=\"'$(Config)'=='Debug' and '$(Platform)'=='Win32'\"><Win32LibraryPath>$(BDS)\\source</Win32LibraryPath></PropertyGroup></Project>",
        );
        write_file(
            &alternate.join("SdkUnit.pas"),
            "unit SdkUnit; interface const ProviderVersion = 701; implementation end.",
        );

        let mut server = TestServer::launch();
        server.initialize(fixture.directory.path(), Value::Null);
        let context = RequestId::from(format!("env-options-context-{send_watcher}"));
        server.send_request(
            context.clone(),
            "pascal/projectContext",
            json!({"textDocument": {"uri": uri(&fixture.main_a)}}),
        );
        let context = server.response(&context);
        assert!(context.error.is_none(), "{context:?}");
        server.send_notification(
            "textDocument/didOpen",
            json!({"textDocument": {"uri": uri(&fixture.main_a), "languageId": "pascal", "version": 1, "text": source}}),
        );

        let initial = RequestId::from(format!("env-options-initial-{send_watcher}"));
        server.send_request(
            initial.clone(),
            "textDocument/definition",
            json!({
                "textDocument": {"uri": uri(&fixture.main_a)},
                "position": position_of(source, "SdkUnit", 0)
            }),
        );
        let initial = server.response(&initial);
        assert!(initial.error.is_none(), "{initial:?}");
        assert_eq!(
            initial.result.as_ref().expect("initial definition")[0]["uri"],
            uri(&fixture.directory.path().join("sdk/7.0/source/SdkUnit.pas")).to_string(),
            "the explicit Config/Platform group must provide the baseline binding"
        );

        write_file(
            &env_options,
            "<Project><PropertyGroup Condition=\"'$(Config)'=='Debug' and '$(Platform)'=='Win32'\"><Win32LibraryPath>$(BDS)\\alternate</Win32LibraryPath></PropertyGroup></Project>",
        );
        if send_watcher {
            server.send_notification(
                "workspace/didChangeWatchedFiles",
                json!({"changes": [{"uri": uri(&env_options), "type": 2}]}),
            );
        }

        let refreshed = RequestId::from(format!("env-options-refreshed-{send_watcher}"));
        server.send_request(
            refreshed.clone(),
            "textDocument/definition",
            json!({
                "textDocument": {"uri": uri(&fixture.main_a)},
                "position": position_of(source, "SdkUnit", 0)
            }),
        );
        let refreshed = server.response(&refreshed);
        assert!(refreshed.error.is_none(), "{refreshed:?}");
        assert_eq!(
            refreshed.result.as_ref().expect("refreshed definition")[0]["uri"],
            uri(&alternate.join("SdkUnit.pas")).to_string(),
            "EnvOptions provider refresh failed (watcher={send_watcher}): {refreshed:?}"
        );
        server.shutdown();
    }
}

#[test]
fn creating_an_absent_explicit_source_recovers_on_demand() {
    let directory = tempfile::tempdir().expect("temporary workspace");
    let root = directory.path();
    let project = root.join("App.dproj");
    let main = root.join("Main.pas");
    let missing = root.join("Missing.pas");
    let source = "unit Main;\ninterface\nuses Missing;\nimplementation\nend.\n";
    write_file(
        &project,
        "<Project><PropertyGroup><MainSource>Main.pas</MainSource></PropertyGroup><ItemGroup><DCCReference Include=\"Missing.pas\" /></ItemGroup></Project>",
    );
    write_file(&main, source);
    let mut server = TestServer::launch();
    server.initialize(root, Value::Null);

    let before = RequestId::from("absent-source-before-create".to_owned());
    server.send_request(
        before.clone(),
        "textDocument/definition",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "Missing", 0)
        }),
    );
    let before = server.response(&before);
    assert!(before.error.is_none(), "{before:?}");
    assert!(before.result.as_ref().is_some_and(Value::is_array));

    write_file(
        &missing,
        "unit Missing;\ninterface\nconst FreshValue = 42;\nimplementation\nend.\n",
    );
    // No watcher event: the retained absent-reference observation must be
    // checked when the next navigation request is prepared.
    let after = RequestId::from("absent-source-after-create".to_owned());
    server.send_request(
        after.clone(),
        "textDocument/definition",
        json!({
            "textDocument": {"uri": uri(&main)},
            "position": position_of(source, "Missing", 0)
        }),
    );
    let after = server.response(&after);
    assert!(after.error.is_none(), "{after:?}");
    assert!(
        after.result.as_ref().is_some_and(|locations| {
            locations.as_array().is_some_and(|locations| {
                locations.len() == 1 && locations[0]["uri"] == uri(&missing).to_string()
            })
        }),
        "creating an explicitly referenced source must restore its binding on demand: {after:?}"
    );
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
fn reset_installation_reports_metadata_selection_consistently() {
    let fixture = selection_fixture();
    let config_path = fixture.directory.path().join(".delphi-tools.local.toml");
    let config = fs::read_to_string(&config_path)
        .expect("read fixture configuration")
        .replace("[projects.\"a/App.dproj\"]\ninstallation='7.0'\n", "")
        .replace("[projects.\"b/App.dproj\"]\ninstallation='37.0'\n", "");
    write_file(&config_path, &config);
    write_file(
        &fixture.project_a,
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource><Platform>Win32</Platform><CompilerVersion>37.0</CompilerVersion></PropertyGroup></Project>",
    );

    let mut server = TestServer::launch();
    server.initialize(fixture.directory.path(), Value::Null);
    let session = RequestId::from("metadata-reset-session-choice".to_owned());
    server.send_request(
        session.clone(),
        "pascal/selectInstallation",
        json!({"projectUri": uri(&fixture.project_a), "installationId": "7.0"}),
    );
    assert!(server.response(&session).error.is_none());

    let reset = RequestId::from("metadata-reset-to-automatic".to_owned());
    server.send_request(
        reset.clone(),
        "pascal/selectInstallation",
        json!({"projectUri": uri(&fixture.project_a), "installationId": null}),
    );
    let reset = server.response(&reset);
    assert!(reset.error.is_none(), "{reset:?}");
    assert_eq!(
        reset.result.as_ref().unwrap()["selectedInstallationId"],
        "37.0"
    );
    assert_eq!(reset.result.as_ref().unwrap()["selectionMode"], "metadata");

    let installation = RequestId::from("metadata-installation-context".to_owned());
    server.send_request(
        installation.clone(),
        "pascal/installationContext",
        json!({"projectUri": uri(&fixture.project_a)}),
    );
    let installation = server.response(&installation);
    assert!(installation.error.is_none(), "{installation:?}");
    let installation = installation.result.unwrap();
    assert_eq!(installation["selectedInstallationId"], "37.0");
    assert_eq!(installation["selectionMode"], "metadata");

    let context = project_context(&mut server, &fixture.main_a, "metadata-reset");
    assert_eq!(context["selectedInstallationId"], "37.0");
    assert_eq!(context["installationSelectionMode"], "metadata");
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

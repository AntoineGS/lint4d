use super::*;
use super::workspace_selection_support::{
    ambiguous_projects, open_automatic_unit, project_context,
    project_context_retry_stale, selection_fixture,
};

#[cfg(feature = "test-support")]
fn two_scope_ambiguous_projects() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let directory = tempfile::tempdir().unwrap();
    let mut values = Vec::new();
    for (scope, first, second) in [("scope-a", "A1", "A2"), ("scope-b", "B1", "B2")] {
        let root = directory.path().join(scope);
        let unit = root.join("src/Unit.pas");
        let first_project = root.join("src").join(format!("{first}.dproj"));
        let second_project = root.join("src").join(format!("{second}.dproj"));
        write_file(&unit, "unit Unit; interface implementation end.");
        let project =
            "<Project><PropertyGroup><MainSource>Unit.pas</MainSource></PropertyGroup></Project>";
        write_file(&first_project, project);
        write_file(&second_project, project);
        values.push((unit, first_project));
    }
    let (unit_a, project_a) = values.remove(0);
    let (unit_b, project_b) = values.remove(0);
    (directory, unit_a, project_a, unit_b, project_b)
}

#[cfg(feature = "test-support")]
#[test]
fn manual_project_choice_supersedes_automatic_worker_before_commit() {
    let (directory, unit, project_a, project_b) = ambiguous_projects();
    let environment = tempfile::tempdir().unwrap();
    let (mut server, barrier) =
        TestServer::launch_with_project_operation_prepared_barrier(environment);
    server.initialize(directory.path(), Value::Null);
    open_automatic_unit(&mut server, &unit);
    wait_for_path(&barrier.entered);
    fs::write(&barrier.release, b"release discovery").unwrap();
    let prompt = server
        .request_with_timeout("window/showMessageRequest", Duration::from_secs(5))
        .expect("automatic project prompt");
    fs::remove_file(&barrier.release).unwrap();

    server.send(Message::Response(Response::new_ok(
        prompt.id,
        prompt.params["actions"][0].clone(),
    )));
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && fs::read(&barrier.entered).map_or(0, |bytes| bytes.len()) < 2
    {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        fs::read(&barrier.entered).unwrap().len() >= 2,
        "automatic selection worker should be held before commit"
    );

    let manual = RequestId::from("manual-choice-fences-held-automatic-choice".to_owned());
    server.send_request(
        manual.clone(),
        "pascal/selectProject",
        json!({"textDocument": {"uri": uri(&unit)}, "projectUri": uri(&project_b)}),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && fs::read(&barrier.entered).map_or(0, |bytes| bytes.len()) < 3
    {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        fs::read(&barrier.entered).unwrap().len() >= 3,
        "manual selection worker should also be held before commit"
    );
    fs::write(&barrier.release, b"release both prepared workers").unwrap();
    let manual_response = server.response(&manual);
    assert!(manual_response.error.is_none(), "{manual_response:?}");

    let mut context = Value::Null;
    for attempt in 0..10 {
        context = project_context(&mut server, &unit, &format!("barrier-{attempt}"));
        if context["selectedProjectUri"] == uri(&project_b).as_str() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(context["selectedProjectUri"], uri(&project_b).as_str());
    assert_ne!(context["selectedProjectUri"], uri(&project_a).as_str());
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn unrelated_scope_manual_selection_does_not_fence_automatic_answer() {
    let (directory, unit_a, project_a, unit_b, project_b) = two_scope_ambiguous_projects();
    let environment = tempfile::tempdir().unwrap();
    let (mut server, barrier) =
        TestServer::launch_with_project_operation_prepared_barrier(environment);
    server.initialize(directory.path(), Value::Null);
    open_automatic_unit(&mut server, &unit_a);
    open_automatic_unit(&mut server, &unit_b);

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && fs::read(&barrier.entered).map_or(0, |bytes| bytes.len()) < 2
    {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(fs::read(&barrier.entered).unwrap().len() >= 2);
    fs::write(&barrier.release, b"release discovery workers").unwrap();
    let prompt = server
        .request_with_timeout("window/showMessageRequest", Duration::from_secs(5))
        .expect("scope prompt");

    let actions = prompt.params["actions"].as_array().unwrap();
    let automatic_is_a = actions.iter().any(|action| action["title"] == "A1.dproj");
    let (automatic_unit, automatic_project, manual_unit, manual_project) = if automatic_is_a {
        (&unit_a, &project_a, &unit_b, &project_b)
    } else {
        (&unit_b, &project_b, &unit_a, &project_a)
    };
    let selected_action = actions
        .iter()
        .find(|action| {
            action["title"]
                == if automatic_is_a {
                    "A1.dproj"
                } else {
                    "B1.dproj"
                }
        })
        .unwrap()
        .clone();
    let manual = RequestId::from("unrelated-scope-manual-selection".to_owned());
    server.send_request(
        manual.clone(),
        "pascal/selectProject",
        json!({"textDocument": {"uri": uri(manual_unit)}, "projectUri": uri(manual_project)}),
    );
    let manual_response = server.response(&manual);
    assert!(manual_response.error.is_none(), "{manual_response:?}");
    let manual_context = project_context(&mut server, manual_unit, "manual-scope-committed");
    assert_eq!(
        manual_context["selectedProjectUri"],
        uri(manual_project).as_str()
    );

    // The prompt remains unanswered while the unrelated scope commits. Its
    // old global PromptKey generation must not discard this still-current
    // scope/candidate choice.
    server.send(Message::Response(Response::new_ok(
        prompt.id,
        selected_action,
    )));
    let mut automatic_context = Value::Null;
    for attempt in 0..10 {
        automatic_context = project_context_retry_stale(
            &mut server,
            automatic_unit,
            &format!("automatic-scope-{attempt}"),
        );
        if automatic_context["selectedProjectUri"] == uri(automatic_project).as_str() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        automatic_context["selectedProjectUri"],
        uri(automatic_project).as_str(),
        "an unrelated scope's manual choice must not invalidate this automatic answer"
    );
    fs::remove_file(&barrier.release).unwrap();
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn unrelated_manual_commit_retries_a_held_automatic_answer_once() {
    let (directory, unit_a, project_a, unit_b, project_b) = two_scope_ambiguous_projects();
    let environment = tempfile::tempdir().unwrap();
    let (mut server, barrier) =
        TestServer::launch_with_automatic_selection_prepared_barrier(environment);
    server.initialize(directory.path(), Value::Null);
    open_automatic_unit(&mut server, &unit_a);
    open_automatic_unit(&mut server, &unit_b);
    let prompt = server
        .request_with_timeout("window/showMessageRequest", Duration::from_secs(5))
        .expect("automatic project choice prompt");
    let actions = prompt.params["actions"].as_array().unwrap();
    let automatic_is_a = actions.iter().any(|action| action["title"] == "A1.dproj");
    let (automatic_unit, automatic_project, manual_unit, manual_project, action_title) =
        if automatic_is_a {
            (&unit_a, &project_a, &unit_b, &project_b, "A1.dproj")
        } else {
            (&unit_b, &project_b, &unit_a, &project_a, "B1.dproj")
        };
    let action = actions
        .iter()
        .find(|action| action["title"] == action_title)
        .unwrap()
        .clone();
    server.send(Message::Response(Response::new_ok(prompt.id, action)));

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !barrier.entered.exists() {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        barrier.entered.exists(),
        "automatic selection should be prepared and held"
    );

    let manual = RequestId::from("commit-other-scope-before-automatic-retry".to_owned());
    server.send_request(
        manual.clone(),
        "pascal/selectProject",
        json!({"textDocument": {"uri": uri(manual_unit)}, "projectUri": uri(manual_project)}),
    );
    let response = server.response(&manual);
    assert!(response.error.is_none(), "{response:?}");
    let manual_context = project_context(&mut server, manual_unit, "other-scope-committed-first");
    assert_eq!(
        manual_context["selectedProjectUri"],
        uri(manual_project).as_str()
    );

    fs::write(&barrier.release, b"release stale automatic snapshot").unwrap();
    let mut automatic_context = Value::Null;
    for attempt in 0..10 {
        automatic_context = project_context_retry_stale(
            &mut server,
            automatic_unit,
            &format!("automatic-retry-{attempt}"),
        );
        if automatic_context["selectedProjectUri"] == uri(automatic_project).as_str() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        automatic_context["selectedProjectUri"],
        uri(automatic_project).as_str(),
        "the stale first snapshot should be revalidated and retried once"
    );
    assert_eq!(
        fs::read(&barrier.entered).unwrap().len(),
        2,
        "automatic selection should use one initial attempt and at most one retry"
    );
    let manual_context =
        project_context_retry_stale(&mut server, manual_unit, "manual-still-selected");
    assert_eq!(
        manual_context["selectedProjectUri"],
        uri(manual_project).as_str()
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn unrelated_commit_during_automatic_validation_retries_validation_once() {
    let (directory, unit_a, project_a, unit_b, project_b) = two_scope_ambiguous_projects();
    let environment = tempfile::tempdir().unwrap();
    let (mut server, barrier) =
        TestServer::launch_with_project_context_prepared_barrier(environment);
    // Let automatic discovery pass; arm the barrier only for answer validation.
    fs::write(&barrier.release, b"initial discovery may pass").unwrap();
    server.initialize(directory.path(), Value::Null);
    open_automatic_unit(&mut server, &unit_a);
    open_automatic_unit(&mut server, &unit_b);
    let prompt = server
        .request_with_timeout("window/showMessageRequest", Duration::from_secs(5))
        .expect("automatic project choice prompt");
    let actions = prompt.params["actions"].as_array().unwrap();
    let automatic_is_a = actions.iter().any(|action| action["title"] == "A1.dproj");
    let (automatic_unit, automatic_project, manual_unit, manual_project, title) = if automatic_is_a
    {
        (&unit_a, &project_a, &unit_b, &project_b, "A1.dproj")
    } else {
        (&unit_b, &project_b, &unit_a, &project_a, "B1.dproj")
    };
    let action = actions
        .iter()
        .find(|action| action["title"] == title)
        .unwrap()
        .clone();
    fs::remove_file(&barrier.release).unwrap();
    server.send(Message::Response(Response::new_ok(prompt.id, action)));
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && fs::read(&barrier.entered).map_or(0, |bytes| bytes.len()) < 3
    {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        fs::read(&barrier.entered).unwrap().len() >= 3,
        "automatic answer validation should be held after the discovery workers"
    );
    let held_validation_count = fs::read(&barrier.entered).unwrap().len();

    let manual = RequestId::from("commit-other-scope-during-validation".to_owned());
    server.send_request(
        manual.clone(),
        "pascal/selectProject",
        json!({"textDocument": {"uri": uri(manual_unit)}, "projectUri": uri(manual_project)}),
    );
    let response = server.response(&manual);
    assert!(response.error.is_none(), "{response:?}");
    fs::write(&barrier.release, b"release validation worker").unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline
        && fs::read(&barrier.entered).map_or(0, |bytes| bytes.len()) < held_validation_count + 1
    {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        fs::read(&barrier.entered).unwrap().len(),
        held_validation_count + 1,
        "only the single bounded validation retry should follow the held attempt"
    );

    let mut context = Value::Null;
    for attempt in 0..10 {
        context = project_context_retry_stale(
            &mut server,
            automatic_unit,
            &format!("validation-retry-{attempt}"),
        );
        if context["selectedProjectUri"] == uri(automatic_project).as_str() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        context["selectedProjectUri"],
        uri(automatic_project).as_str()
    );
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn cancelling_pending_manual_selection_resumes_deferred_automatic_answer() {
    let (directory, unit, project_a, project_b) = ambiguous_projects();
    let environment = tempfile::tempdir().unwrap();
    let (mut server, barrier) =
        TestServer::launch_with_manual_selection_prepared_barrier(environment);
    server.initialize(directory.path(), Value::Null);
    open_automatic_unit(&mut server, &unit);
    let prompt = server
        .request_with_timeout("window/showMessageRequest", Duration::from_secs(5))
        .expect("automatic project choice prompt");

    let manual = RequestId::from("cancel-pending-same-scope-manual-selection".to_owned());
    server.send_request(
        manual.clone(),
        "pascal/selectProject",
        json!({"textDocument": {"uri": uri(&unit)}, "projectUri": uri(&project_b)}),
    );
    wait_for_path(&barrier.entered);
    server.send(Message::Response(Response::new_ok(
        prompt.id,
        prompt.params["actions"][0].clone(),
    )));

    // Let the automatic answer's fresh-context worker complete while the manual
    // operation remains held; this is the state that must not be stranded.
    thread::sleep(Duration::from_millis(100));

    server.send_notification("$/cancelRequest", json!({"id": manual}));
    let cancelled = server.response(&manual);
    assert_eq!(cancelled.error.unwrap().code, -32800);

    let mut context = Value::Null;
    for attempt in 0..20 {
        context = project_context_retry_stale(
            &mut server,
            &unit,
            &format!("after-manual-cancel-{attempt}"),
        );
        if context["selectedProjectUri"] == uri(&project_a).as_str() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        context["selectedProjectUri"],
        uri(&project_a).as_str(),
        "canceling the last manual intent must resume a fresh automatic validation"
    );
    fs::write(&barrier.release, b"release canceled worker cleanup").unwrap();
    server.shutdown();
}

#[cfg(feature = "test-support")]
#[test]
fn navigation_response_is_rejected_after_installation_switch() {
    let fixture = selection_fixture();
    let main_source = "unit Main;\ninterface\nuses SdkUnit;\nimplementation\nend.\n";
    write_file(&fixture.main_a, main_source);
    write_file(
        &fixture.project_a,
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource><Platform>Win32</Platform><DCC_UnitSearchPath>../sdk/7.0/source</DCC_UnitSearchPath></PropertyGroup></Project>",
    );
    let environment = tempfile::tempdir().expect("isolated test environment");
    let (mut server, barrier) = TestServer::launch_with_navigation_barrier(environment);
    server.initialize(fixture.directory.path(), Value::Null);

    let definition = RequestId::from("definition-before-installation-switch".to_owned());
    server.send_request(
        definition.clone(),
        "textDocument/definition",
        json!({
            "textDocument": {"uri": uri(&fixture.main_a)},
            "position": position_of(main_source, "SdkUnit", 0)
        }),
    );
    barrier.wait_until_entered();

    let select = RequestId::from("switch-during-navigation".to_owned());
    server.send_request(
        select.clone(),
        "pascal/selectInstallation",
        json!({"projectUri": uri(&fixture.project_a), "installationId": "37.0"}),
    );
    let selected = server.response(&select);
    assert!(selected.error.is_none(), "{selected:?}");
    barrier.release();

    let stale = server.response(&definition);
    assert!(
        stale.error.is_some(),
        "navigation captured before installation selection must be discarded: {stale:?}"
    );
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

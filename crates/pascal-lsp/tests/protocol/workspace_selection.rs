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

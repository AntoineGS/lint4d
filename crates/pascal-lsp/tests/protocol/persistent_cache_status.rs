use super::*;

fn cache_status(server: &mut TestServer) -> Value {
    let id = RequestId::from("cache-status".to_string());
    server.send_request(id.clone(), "pascal/cacheStatus", json!({}));
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    response.result.unwrap()
}

#[test]
fn cache_status_reports_the_default_store_under_the_test_cache_home() {
    let (_temp, main, _provider, _main_source, _provider_source) = standard_workspace();
    let environment = tempfile::tempdir().unwrap();
    let mut server = TestServer::launch_with_environment_path(environment.path());
    server.initialize(main.parent().unwrap(), Value::Null);
    let status = cache_status(&mut server);
    assert!(
        matches!(
            status["state"].as_str(),
            Some("opening" | "enabled" | "bypassed")
        ),
        "{status}"
    );
    assert_eq!(
        status["directory"],
        environment
            .path()
            .join("cache/lint4d/analysis")
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(status["limits"]["pageBytes"], 16 << 20);
    assert!(status.to_string().len() <= 8192);
    server.shutdown();
}

fn index_status(server: &mut TestServer, tag: usize) -> Value {
    let id = RequestId::from(format!("index-status-{tag}"));
    server.send_request(id.clone(), "pascal/indexStatus", json!({}));
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    response.result.unwrap()
}

#[test]
fn index_status_answers_before_the_index_exists_and_names_no_member() {
    let (_temp, main, provider, main_source, _provider_source) = standard_workspace();
    let environment = tempfile::tempdir().unwrap();
    let mut server = TestServer::launch_with_environment_path(environment.path());
    server.initialize(main.parent().unwrap(), Value::Null);
    assert_eq!(
        index_status(&mut server, 0),
        json!({"schema": 1, "contexts": []})
    );
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": Url::from_file_path(&main).unwrap(),
            "languageId": "pascal",
            "version": 1,
            "text": main_source,
        }}),
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut tag = 1;
    let status = loop {
        let status = index_status(&mut server, tag);
        tag += 1;
        let done = status["contexts"]
            .as_array()
            .is_some_and(|contexts| contexts.iter().any(|context| context["complete"] == true));
        if done || Instant::now() >= deadline {
            break status;
        }
    };
    let text = status.to_string();
    assert!(text.len() <= 8192, "{text}");
    assert_eq!(status["schema"], 1, "{text}");
    let context = status["contexts"]
        .as_array()
        .and_then(|contexts| contexts.iter().find(|context| context["complete"] == true))
        .unwrap_or_else(|| panic!("no complete context: {text}"));
    assert!(context["members"].as_u64().unwrap() >= 2, "{text}");
    assert_eq!(context["pending"], 0, "{text}");
    assert!(
        context["anchor"]["document"].is_string() || context["anchor"]["project"].is_string(),
        "{text}"
    );
    assert!(!text.contains("Provider.pas"), "{text}");
    assert!(
        !text.contains(&provider.to_string_lossy().to_string()),
        "{text}"
    );
    server.shutdown();
}

#[test]
fn a_disabled_cache_reports_disabled_and_creates_nothing() {
    let (_temp, main, _provider, _main_source, _provider_source) = standard_workspace();
    let environment = tempfile::tempdir().unwrap();
    let mut server = TestServer::launch_with_environment_path(environment.path());
    server.initialize(
        main.parent().unwrap(),
        json!({"persistentCacheEnabled": false}),
    );
    assert_eq!(cache_status(&mut server)["state"], "disabled");
    server.shutdown();
    assert!(!environment.path().join("cache").exists());
}

#[test]
fn max_workspace_units_zero_rejects_initialize() {
    let temp = tempfile::tempdir().unwrap();
    let mut server = TestServer::launch();
    let id = RequestId::from("initialize".to_string());
    server.send_request(
        id.clone(),
        "initialize",
        json!({
            "processId": null,
            "rootUri": Url::from_file_path(temp.path()).unwrap(),
            "initializationOptions": {"maxWorkspaceUnits": 0},
            "capabilities": {}
        }),
    );
    let error = server.response(&id).error.expect("rejected");
    assert_eq!(error.code, -32602);
    assert_eq!(
        error.message,
        "initializationOptions.maxWorkspaceUnits must be at least 1"
    );
    // The server waits for a valid initialize (`run_connection` loops);
    // finish the session normally.
    server.initialize(temp.path(), Value::Null);
    server.shutdown();
}
